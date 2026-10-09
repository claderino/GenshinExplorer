//! Map data from the official HoYoLab interactive-map API (international).
//!
//! Every map (2/7/9/34/36/37/40) serves a `detail_v2` tile pyramid:
//!  * `map_version` + zoom range (`min_zoom..=0`, zoom 0 = native res)
//!  * 256 px Leaflet-style grid at
//!    `map_manage/map/{id}/{version}/{x}_{y}_{P|N}{z}.png`
//!
//! `ensure_map` pre-downloads a low-zoom overview for an instant backdrop;
//! sharper tiles stream in on demand via `tiles::TileStore`.
//!
//! World→map (verified by player movement):
//!     map_x = origin_x − world_z,  map_y = origin_y − world_x
//! Pin API coordinates are origin-relative (may be negative):
//!     pin_map_px = pin + origin

use std::path::Path;

use anyhow::{Context, Result, bail};
use eframe::egui;

use crate::tiles;

const MAP_INFO_API: &str =
    "https://sg-public-api-static.hoyolab.com/common/map_user/ys_obc/v3/map/info";
/// Max tiles for the pre-stitched overview backdrop.
const OVERVIEW_MAX_TILES: u32 = 400;

pub struct MapData {
    pub map_id: u32,
    pub name: String,
    /// Low-res stitched backdrop (RGBA).
    pub color_image: egui::ColorImage,
    pub total_size: (f64, f64),
    pub origin: (f64, f64),
    /// Builtin canvas scale (px per world unit); the calibrator overrides.
    pub world_scale: f64,
    pub version: String,
    pub min_zoom: i32,
    pub max_zoom: i32,
}

/// Empirically solved canvas scales (px per world unit); default 1.0.
/// The in-app multi-point calibrator can override these per map.
pub fn builtin_world_scale(map_id: u32) -> f64 {
    match map_id {
        40 => 0.574_166_454_363_676_7, // Frost Moon (user-verified 5-pt solve)
        _ => 1.0,
    }
}

impl MapData {
    /// World coordinates → RAW map frame (origin − world), unscaled.
    /// Scaling + offset are applied by the calibration transform.
    pub fn world_to_map(&self, x: f32, z: f32) -> (f64, f64) {
        (self.origin.0 - z as f64, self.origin.1 - x as f64)
    }
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .build()
}

fn arr2(v: &serde_json::Value, key: &'static str) -> Result<(f64, f64)> {
    let a = v.get(key).and_then(|v| v.as_array()).context(key)?;
    Ok((a[0].as_f64().context("x")?, a[1].as_f64().context("y")?))
}

/// Downloads the map overview on first run; tiles cache on disk afterwards.
/// `progress(done, total)` reports overview-tile progress.
pub fn ensure_map(
    cache_dir: &Path,
    map_id: u32,
    progress: &dyn Fn(usize, usize),
) -> Result<MapData> {
    std::fs::create_dir_all(cache_dir)?;

    // 1. Map info (full API body, shared with pins.rs).
    let info_path = cache_dir.join(format!("info_v3_{map_id}.json"));
    let info: serde_json::Value = if info_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&info_path)?)?
    } else {
        let url = format!("{MAP_INFO_API}?map_id={map_id}&app_sn=ys_obc&lang=en-us");
        let mut reader = http_agent().get(&url).call().context("fetching map info")?.into_reader();
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut reader, &mut buf)?;
        let body: serde_json::Value = serde_json::from_str(&buf)?;
        std::fs::write(&info_path, body.to_string())?;
        body
    };

    let name = info.pointer("/data/info/name")
        .and_then(|v| v.as_str()).unwrap_or("Map").to_string();
    let v2 = info.pointer("/data/info/detail_v2").filter(|v| !v.is_null());
    if v2.is_none() {
        bail!("map {map_id} has no detail_v2 tile pyramid");
    }
    let v2 = v2.unwrap();

    let total = arr2(v2, "total_size")?;
    let origin = arr2(v2, "origin")?;
    let version = v2.get("map_version").and_then(|v| v.as_str())
        .context("map_version")?.to_string();
    let min_zoom = v2.get("min_zoom").and_then(|v| v.as_i64()).unwrap_or(-4) as i32;
    let max_zoom = v2.get("max_zoom").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

    // 2. Overview backdrop: lowest zoom that fits the tile budget.
    let mut zoom = min_zoom;
    loop {
        let (c, r) = tiles::grid_for(total, zoom);
        if c * r <= OVERVIEW_MAX_TILES || zoom >= max_zoom {
            break;
        }
        zoom += 1;
    }
    let (cols, rows) = tiles::grid_for(total, zoom);
    let total_tiles = (cols * rows) as usize;

    // 3. Download the overview grid (same disk cache as on-demand tiles).
    let ztag = if zoom < 0 { format!("N{}", -zoom) } else { format!("P{zoom}") };
    let mut paths: Vec<std::path::PathBuf> = Vec::with_capacity(total_tiles);
    let mut done = 0usize;
    for y in 0..rows {
        for x in 0..cols {
            let path = cache_dir.join(format!("tilev2_{map_id}_{zoom}_{x}_{y}.png"));
            if !path.exists() {
                let url = format!("{}{map_id}/{version}/{x}_{y}_{ztag}.png", tiles::TILE_BASE);
                let mut reader = http_agent().get(&url).call()
                    .with_context(|| format!("tile {x},{y} z{zoom}"))?.into_reader();
                let mut buf = Vec::new();
                std::io::Read::read_to_end(&mut reader, &mut buf)?;
                std::fs::write(&path, buf.as_slice())?;
            }
            done += 1;
            progress(done, total_tiles);
            paths.push(path);
        }
    }

    // 4. Stitch at that grid's native resolution (total * 2^zoom).
    let f = 2f64.powi(zoom);
    let out_w = (total.0 * f).round() as u32;
    let out_h = (total.1 * f).round() as u32;
    let mut out = vec![egui::Color32::TRANSPARENT; (out_w * out_h) as usize];
    for (i, path) in paths.iter().enumerate() {
        let (x0, y0) = ((i as u32 % cols) as u32 * 256, (i as u32 / cols) as u32 * 256);
        let img = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
        let rgba = img.to_rgba8();
        for (y, line) in rgba.rows().enumerate() {
            let dy = y0 + y as u32;
            if dy >= out_h { break; }
            for (x, px) in line.enumerate() {
                let dx = x0 + x as u32;
                if dx >= out_w { break; }
                let [r, g, b, a] = px.0;
                out[(dy * out_w + dx) as usize] =
                    egui::Color32::from_rgba_unmultiplied(r, g, b, a);
            }
        }
    }
    if out_w == 0 || out_h == 0 {
        bail!("stitched map empty");
    }

    tracing::info!(map_id, name, zoom, cols, rows, "map overview ready");
    Ok(MapData {
        map_id,
        name,
        color_image: egui::ColorImage {
            size: [out_w as usize, out_h as usize],
            source_size: egui::Vec2::new(out_w as f32, out_h as f32),
            pixels: out,
        },
        total_size: total,
        origin,
        world_scale: builtin_world_scale(map_id),
        version,
        min_zoom,
        max_zoom,
    })
}
