//! On-demand 256 px tile pyramid from the HoYoLab CDN (`detail_v2` scheme).
//!
//! Tiles are addressed `(map_id, zoom, x, y)`; zoom 0 is native resolution
//! (1 tile px = 1 map px), negative zooms are half-res per level. The store
//! downloads tiles in background worker threads to a disk cache and hands
//! decoded textures to the UI thread with an LRU byte budget.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui;

pub const TILE_BASE: &str = "https://act-webstatic.hoyoverse.com/map_manage/map/";
pub const TILE_PX: f64 = 256.0;

pub type TileKey = (u32, i32, u32, u32);

struct TileJob {
    key: TileKey,
    url: String,
    path: PathBuf,
}

struct Shared {
    queue: Mutex<VecDeque<TileJob>>,
    done: Mutex<Vec<TileKey>>,
    pending: Mutex<HashSet<TileKey>>,
}

impl Shared {
    fn request(&self, job: TileJob) {
        if job.path.exists() {
            // Already on disk — still route through `done` so the UI loads it.
            let mut done = self.done.lock().unwrap();
            if !done.contains(&job.key) {
                done.push(job.key);
            }
            return;
        }
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.contains(&job.key) {
                return;
            }
            pending.insert(job.key);
        }
        self.queue.lock().unwrap().push_back(job);
    }
    fn finished(&self, key: TileKey) {
        self.pending.lock().unwrap().remove(&key);
        self.done.lock().unwrap().push(key);
    }
}

/// Tile URL for the v2 scheme: `.../{map_id}/{version}/{x}_{y}_{P0|N2}.png`.
pub fn tile_url(map_id: u32, version: &str, x: u32, y: u32, zoom: i32) -> String {
    let ztag = if zoom < 0 { format!("N{}", -zoom) } else { format!("P{zoom}") };
    format!("{TILE_BASE}{map_id}/{version}/{x}_{y}_{ztag}.png")
}

/// Grid dimensions for a zoom level.
pub fn grid_for(total: (f64, f64), zoom: i32) -> (u32, u32) {
    let f = 2f64.powi(zoom);
    (
        (total.0 * f / TILE_PX).ceil() as u32,
        (total.1 * f / TILE_PX).ceil() as u32,
    )
}

pub struct TileStore {
    cache_dir: PathBuf,
    shared: Arc<Shared>,
    textures: HashMap<TileKey, (egui::TextureHandle, u64)>,
    frame: u64,
    bytes: usize,
    budget: usize,
}

impl TileStore {
    pub fn new(cache_dir: std::path::PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            done: Mutex::new(Vec::new()),
            pending: Mutex::new(HashSet::new()),
        });
        // Two persistent download workers.
        for i in 0..2 {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name(format!("tile-dl-{i}"))
                .spawn(move || worker(shared))
                .ok();
        }
        Self {
            cache_dir,
            shared,
            textures: HashMap::new(),
            frame: 0,
            bytes: 0,
            budget: 384 * 1024 * 1024, // ~1500 tiles of 256 KB
        }
    }

    pub fn has_pending(&self) -> bool {
        let q = self.shared.queue.lock().unwrap().len();
        let d = self.shared.done.lock().unwrap().len();
        let p = self.shared.pending.lock().unwrap().len();
        q + d + p > 0
    }

    /// Drain finished downloads into textures. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        let finished: Vec<TileKey> = std::mem::take(&mut *self.shared.done.lock().unwrap());
        for key in finished {
            let path = self.tile_path(key);
            match image::open(&path) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let ci = egui::ColorImage::from_rgba_unmultiplied(
                        [w as usize, h as usize], &rgba);
                    let size = ci.pixels.len() * 4;
                    if let Some(old) = self.textures.insert(
                        key,
                        (ctx.load_texture(format!("tile_{key:?}"), ci, egui::TextureOptions::LINEAR),
                         self.frame),
                    ) {
                        self.bytes -= old.0.size_vec2().x as usize
                            * old.0.size_vec2().y as usize * 4;
                    }
                    self.bytes += size;
                }
                Err(e) => {
                    tracing::warn!("tile decode failed {}: {e}", path.display());
                    let _ = std::fs::remove_file(&path); // poison cache entry
                }
            }
        }
        self.evict();
    }

    fn evict(&mut self) {
        if self.bytes <= self.budget {
            return;
        }
        // Evict least-recently-used textures not touched this frame.
        let mut candidates: Vec<TileKey> = self
            .textures
            .iter()
            .filter(|(_, (_, f))| *f != self.frame)
            .map(|(k, _)| *k)
            .collect();
        candidates.sort_by_key(|k| self.textures[k].1);
        for key in candidates {
            if self.bytes <= self.budget {
                break;
            }
            if let Some((tex, _)) = self.textures.remove(&key) {
                let s = tex.size_vec2();
                self.bytes -= s.x as usize * s.y as usize * 4;
            }
        }
    }

    fn tile_path(&self, (map_id, zoom, x, y): TileKey) -> PathBuf {
        self.cache_dir.join(format!("tilev2_{map_id}_{zoom}_{x}_{y}.png"))
    }

    /// Fetch (or schedule) a tile; returns its texture if resident.
    pub fn get(
        &mut self,
        map_id: u32,
        version: &str,
        zoom: i32,
        x: u32,
        y: u32,
    ) -> Option<egui::TextureId> {
        let key: TileKey = (map_id, zoom, x, y);
        if let Some((tex, last)) = self.textures.get_mut(&key) {
            *last = self.frame;
            return Some(tex.id());
        }
        // Schedule a fetch (no-op when cached/pending).
        self.shared.request(TileJob {
            key,
            url: tile_url(map_id, version, x, y, zoom),
            path: self.tile_path(key),
        });
        None
    }
}

fn worker(shared: Arc<Shared>) {
    let agent = ureq::AgentBuilder::new()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .build();
    loop {
        let job = shared.queue.lock().unwrap().pop_front();
        let Some(job) = job else {
            std::thread::sleep(std::time::Duration::from_millis(40));
            continue;
        };
        match agent.get(&job.url).call() {
            Ok(resp) => {
                let mut reader = resp.into_reader();
                let mut buf = Vec::new();
                if std::io::Read::read_to_end(&mut reader, &mut buf).is_ok() {
                    let tmp = job.path.with_extension("part");
                    if std::fs::write(&tmp, &buf).is_ok() {
                        let _ = std::fs::rename(&tmp, &job.path);
                    }
                }
            }
            Err(e) => tracing::debug!("tile {} failed: {e}", job.url),
        }
        shared.finished(job.key);
    }
}
