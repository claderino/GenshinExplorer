//! Docked map panel with HoYoLab pins, individual label toggles, icons, and
//! on-demand 256 px tile streaming (Leaflet-style, like the official map).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use eframe::egui;

use crate::cookies;
use crate::map::{self, MapData};
use crate::pins::{self, PinCategory, PinData};
use crate::sync;
use crate::tiles::{self, TileStore};

#[derive(Clone)]
pub struct ChestMark { pub x: f32, pub y: f32, pub z: f32, pub amount: i64, pub kind: String }

/// Canvas-originated calibration actions.
#[derive(Clone, Copy)]
enum CalAction {
    AddPoint { mx: f64, my: f64 },
    Clear,
}

/// A user-collected calibration correspondence: the player's world position
/// when they clicked, and the canvas pixel they marked as their true spot.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct CalPoint {
    pub wx: f32,
    pub wz: f32,
    pub mx: f64,
    pub my: f64,
    /// Scene generation when the point was captured (layers are separate
    /// scenes; each scene change bumps the generation).
    #[serde(default)]
    pub scene_gen: u32,
}

/// Effective world→canvas transform (scale per axis + offset).
#[derive(Clone, Copy)]
pub struct Xform {
    pub sx: f64,
    pub sy: f64,
    pub ox: f64,
    pub oy: f64,
}

impl Xform {
    pub fn apply(&self, origin: (f64, f64), x: f32, z: f32) -> (f64, f64) {
        (
            (origin.0 - z as f64) * self.sx + self.ox,
            (origin.1 - x as f64) * self.sy + self.oy,
        )
    }
    pub fn identity(builtin: f64) -> Self {
        Self { sx: builtin, sy: builtin, ox: 0.0, oy: 0.0 }
    }
}

/// Least-squares solve of (scale, offset) per axis from one frame's
/// calibration points. Returns the transform and the max residual (px).
fn solve_frame(
    points: &[CalPoint],
    origin: (f64, f64),
    builtin: f64,
) -> (Xform, f64) {
    if points.is_empty() {
        return (Xform::identity(builtin), 0.0);
    }
    // u = origin.0 − wz (x-axis), v = origin.1 − wx (y-axis)
    let us: Vec<f64> = points.iter().map(|p| origin.0 - p.wz as f64).collect();
    let vs: Vec<f64> = points.iter().map(|p| origin.1 - p.wx as f64).collect();
    let xs: Vec<f64> = points.iter().map(|p| p.mx).collect();
    let ys: Vec<f64> = points.iter().map(|p| p.my).collect();
    let n = points.len() as f64;

    let fit = |u: &[f64], t: &[f64], default_s: f64| -> (f64, f64) {
        let su: f64 = u.iter().sum();
        let st: f64 = t.iter().sum();
        let suu: f64 = u.iter().map(|a| a * a).sum();
        let sut: f64 = u.iter().zip(t).map(|(a, b)| a * b).sum();
        let det = n * suu - su * su;
        // Degenerate (points share this axis' coordinate) → offset-only.
        if det < 1.0e6 {
            let c = (st - default_s * su) / n;
            return (default_s, c);
        }
        let s = (n * sut - su * st) / det;
        let c = (st - s * su) / n;
        (s, c)
    };

    let (sx, ox) = fit(&us, &xs, builtin);
    let (sy, oy) = fit(&vs, &ys, builtin);
    let xf = Xform { sx, sy, ox, oy };
    let maxres = points
        .iter()
        .map(|p| {
            let (px, py) = xf.apply(origin, p.wx, p.wz);
            ((px - p.mx).powi(2) + (py - p.my).powi(2)).sqrt()
        })
        .fold(0.0_f64, f64::max);
    (xf, maxres)
}

/// Solve all frames with ONE shared scale per map (canvas px per world
/// unit is a property of the canvas, not the layer) and per-frame offsets.
/// Returns per-frame transforms + max residuals.
fn solve_all_frames(
    frames: &[Vec<CalPoint>],
    origin: (f64, f64),
    builtin: f64,
) -> Vec<(Xform, f64)> {
    // ── Global scale from ALL points (least squares per axis). ──
    // Scale is fit from WITHIN-FRAME spread (per-frame centering): layers
    // share the canvas scale but have different offsets, so pooling raw
    // values would corrupt the slope. Single-point frames contribute
    // nothing (no spread) — harmless.
    let all: Vec<&CalPoint> = frames.iter().flatten().collect();
    let (sx, sy) = if all.len() >= 2 {
        let fit = |proj: &dyn Fn(&CalPoint) -> (f64, f64)| -> f64 {
            // Center u and target within each frame.
            let mut us: Vec<f64> = Vec::new();
            let mut ts: Vec<f64> = Vec::new();
            for f in frames {
                if f.len() < 2 {
                    continue; // no within-frame spread
                }
                let fu: Vec<f64> =
                    f.iter().map(|p| proj(p).0).collect();
                let ft: Vec<f64> =
                    f.iter().map(|p| proj(p).1).collect();
                let mu: f64 = fu.iter().sum::<f64>() / fu.len() as f64;
                let mt: f64 = ft.iter().sum::<f64>() / ft.len() as f64;
                us.extend(fu.iter().map(|u| u - mu));
                ts.extend(ft.iter().map(|t| t - mt));
            }
            let n = us.len() as f64;
            if n < 1.0 {
                return builtin; // no frame has spread
            }
            let suu: f64 = us.iter().map(|a| a * a).sum();
            let sut: f64 = us.iter().zip(&ts).map(|(a, b)| a * b).sum();
            let sigma = suu.sqrt() / n;
            if sigma < 150.0 || suu < 1.0e-9 {
                builtin // not enough within-frame spread
            } else {
                sut / suu
            }
        };
        let sx = fit(&|p| (origin.0 - p.wz as f64, p.mx));
        let sy = fit(&|p| (origin.1 - p.wx as f64, p.my));
        (sx, sy)
    } else {
        (builtin, builtin)
    };
    // ── Per-frame offsets (mean residual; a single point suffices). ──
    frames
        .iter()
        .map(|f| {
            if f.is_empty() {
                return (Xform { sx, sy, ox: 0.0, oy: 0.0 }, 0.0);
            }
            let n = f.len() as f64;
            let ox: f64 = f
                .iter()
                .map(|p| p.mx - sx * (origin.0 - p.wz as f64))
                .sum::<f64>()
                / n;
            let oy: f64 = f
                .iter()
                .map(|p| p.my - sy * (origin.1 - p.wx as f64))
                .sum::<f64>()
                / n;
            let xf = Xform { sx, sy, ox, oy };
            let maxres = f
                .iter()
                .map(|p| {
                    let (px, py) = xf.apply(origin, p.wx, p.wz);
                    ((px - p.mx).powi(2) + (py - p.my).powi(2)).sqrt()
                })
                .fold(0.0_f64, f64::max);
            (xf, maxres)
        })
        .collect()
}

/// Assign a world position to a calibration frame. Layers are separate
/// scenes: same-scene-generation points get priority (nearest within
/// `SAME_GEN_RADIUS`), then any frame with a point within
/// `REJOIN_RADIUS` (returning to a known area), else None.
fn assign_frame(
    frames: &[Vec<CalPoint>],
    x: f32,
    z: f32,
    scene_gen: u32,
) -> Option<usize> {
    let nearest = |f: &Vec<CalPoint>| -> f64 {
        f.iter()
            .map(|p| {
                (((p.wx - x) as f64).powi(2)
                    + ((p.wz - z) as f64).powi(2))
                    .sqrt()
            })
            .fold(f64::INFINITY, f64::min)
    };
    // 1. Same scene generation.
    let mut best: Option<(usize, f64)> = None;
    for (i, f) in frames.iter().enumerate() {
        if f.iter().any(|p| p.scene_gen == scene_gen) {
            let d = nearest(f);
            if d < 3000.0 && d < best.map(|b| b.1).unwrap_or(f64::INFINITY) {
                best = Some((i, d));
            }
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }
    // 2. Any frame with a close point (re-entry).
    for (i, f) in frames.iter().enumerate() {
        if nearest(f) < 800.0 {
            return Some(i);
        }
    }
    None
}

/// A pin clicked on the map (details popup).
#[derive(Clone)]
pub struct SelectedPin { pub map_id: u32, pub pin_id: u64, pub label_id: u32 }

pub enum MapLoadState {
    Idle,
    Downloading {
        done: Arc<AtomicUsize>,
        total: Arc<AtomicUsize>,
        result: Arc<Mutex<Option<anyhow::Result<MapData>>>>,
    },
    Ready { texture: Option<egui::TextureHandle>, data: Arc<MapData> },
}

enum PinState { NotLoaded, Loading(Arc<Mutex<Option<anyhow::Result<PinData>>>>), Loaded(Arc<PinData>) }

pub struct MapWindow {
    pub state: MapLoadState,
    pub open: bool,
    pan: egui::Vec2,
    /// Screen pixels per map pixel (1.0 = native tile resolution).
    zoom: f32,
    view_init: bool,
    follow: bool,
    pub calibrate_offset: Option<(f64, f64)>, calibrating: bool,
    pin_state: PinState,
    pin_filter: pins::FilterState,
    icon_textures: HashMap<u32, egui::TextureHandle>,
    icons_initialized: bool,
    selected_map: u32,
    tiles: Option<TileStore>,
    /// Pin currently selected (details popup open).
    selected: Option<SelectedPin>,
    /// Collected pin ids keyed (uid, map_id) — persisted.
    completed: HashMap<(u32, u32), std::collections::HashSet<u64>>,
    /// UID whose bucket is active (None → uid 0 bucket until detected).
    active_uid: Option<u32>,
    /// Chest opens detected via packets, waiting to be matched to API pins.
    pending_chests: Vec<ChestMark>,
    /// Oculus collections (player x,z when the gadget config id appeared).
    pending_oculi: Vec<(f32, f32)>,
    /// Challenge completions (20234-shape success) at the player position.
    pending_challenges: Vec<(f32, f32)>,
    /// Teleport arrivals — used for floor auto-selection via pin ownership.
    pending_teleports: Vec<(f32, f32, f32, f32)>,
    /// Recent challenge completions (map px) — chests opening near these are
    /// the spawned reward chest, not a pinned chest.
    recent_challenge_done: Vec<(f64, f64, std::time::Instant)>,
    /// Request from the pin popup to center the view on a pin (map px).
    center_request: Option<(f64, f64)>,
    /// Sidebar label search query.
    label_search: String,
    /// Auto-switch sub-map when the player enters its canvas.
    auto_map: bool,
    /// Manual dropdown pick locks the selected map — auto-switching
    /// stands down until a region packet/scene id fires or the
    /// 🗺 auto toggle is cycled.
    manual_map_lock: bool,
    /// Region-broadcast switch (authoritative, from packets).
    pending_region: Option<u32>,
    /// While a region broadcast is active (non-Teyvat), geometric
    /// auto-switching stands down — packets outrank canvas guessing.
    region_lock: Option<u32>,
    /// Minimap layer report (5991) awaiting resolution to a floor.
    /// Resolved via the learned mapping, or learned from geometry when
    /// the position enters a floor overlay.
    pending_layer: Option<u64>,
    /// When the last packet-driven layer decision happened (floor set or
    /// surface lock) — teleport pin-matching stands down for a short
    /// window after, since both fire around the same transition and the
    /// layer packet outranks the pin heuristic.
    layer_authoritative_at: Option<std::time::Instant>,
    /// The layer id most recently applied (for the mapping manager UI
    /// and manual fix-ups).
    active_layer: Option<u64>,
    /// Layer-mapping manager window open.
    layers_open: bool,
    /// Learned map-layer id → floor identity (group_id, floor_id),
    /// persisted per map in `learned_map_layers.json`.
    learned_layers: std::collections::HashMap<u32, std::collections::HashMap<u64, (u32, u32)>>,
    /// Dirty flag for the learned-layers save.
    learned_layers_dirty: bool,
    // ── HoYoLab import ──
    sync_open: bool,
    sync_cookie: String,
    sync_status: Option<String>,
    /// Mirror local mark/unmark actions to HoYoLab automatically.
    pub sync_auto: bool,
    /// Saved cookie profiles (DPAPI-encrypted on disk).
    sync_profiles: Vec<crate::cookies::CookieProfile>,
    /// Selected profile index (None = ad-hoc session cookie).
    sync_profile_idx: Option<usize>,
    /// Name/uid inputs for saving a profile.
    sync_profile_name: String,
    sync_profile_uid: String,
    /// Sessions found by the last 🧲 detection.
    sync_detected: Vec<cookies::FoundCookie>,
    /// Selected entry in the detected list (entries from one login share a
    /// cookie — selection must be tracked by index, not by cookie).
    sync_detected_idx: usize,
    /// Background 🧲 job (browser scan + per-session role lookup).
    detect_job: Option<Arc<Mutex<Option<Vec<cookies::FoundCookie>>>>>,
    sync_job: Option<Arc<Mutex<Option<anyhow::Result<sync::MarksResult>>>>>,
    /// Export job: (progress text, result) — pushes local pins to HoYoLab.
    export_job: Option<(
        Arc<Mutex<String>>,
        Arc<Mutex<Option<anyhow::Result<(usize, usize)>>>>,
    )>,
    /// Player position history (x, z, when) ~2s cadence, for challenge
    /// dwell detection.
    pos_history: Vec<(f32, f32, std::time::Instant)>,
    /// Calibration points grouped into frames (layers). Frame 0 = the
    /// base layer; further frames appear when calibrating in areas with a
    /// different coordinate frame (auto-created by distance).
    cal_points: Vec<Vec<CalPoint>>,
    /// Index of the frame the live position is currently assigned to.
    active_cal_frame: usize,
    /// Active floor (layer) index into PinData::floors; None = surface.
    active_floor: Option<usize>,
    /// True when the user manually selected a floor — auto-follow stands
    /// down until they return to the surface.
    floor_locked: bool,
    /// Scene generation — bumped on every detected scene change (layers
    /// are separate scenes). Calibration points remember their generation.
    pub scene_gen: u32,
    /// Floor overlay textures (floor_id → texture), lazily downloaded.
    floor_overlays: HashMap<u32, egui::TextureHandle>,
    /// Solved per-axis scale override (from ≥2 calibration points).
    cal_scale: Option<(f64, f64)>,
    /// Transient toast notes ("✓ Exquisite Chest auto-collected").
    auto_notes: Vec<(String, std::time::Instant)>,
}

impl MapWindow {
    pub fn new() -> Self {
        let selected_map = 2;
        let (cal_off, cal_scale, cal_points) =
            Self::load_calibration(selected_map);
        Self {
            state: MapLoadState::Idle, open: true,
            pan: egui::Vec2::ZERO, zoom: 0.05, view_init: false, follow: true,
            calibrate_offset: cal_off, calibrating: false,
            cal_points,
            active_cal_frame: 0,
            active_floor: None,
            floor_locked: false,
            scene_gen: 0,
            floor_overlays: HashMap::new(),
            cal_scale,
            pin_state: PinState::NotLoaded,
            pin_filter: pins::FilterState::load(&Self::data_dir(), 0, selected_map),
            icon_textures: HashMap::new(), icons_initialized: false,
            selected_map,
            tiles: None,
            selected: None,
            completed: Self::load_completed(),
            active_uid: None,
            pending_chests: Vec::new(),
            pending_oculi: Vec::new(),
            pending_challenges: Vec::new(),
            pending_teleports: Vec::new(),
            recent_challenge_done: Vec::new(),
            sync_open: false,
            sync_cookie: String::new(),
            sync_status: None,
            sync_auto: false,
            sync_profiles: crate::cookies::load_profiles(),
            sync_profile_idx: None,
            sync_profile_name: String::new(),
            sync_profile_uid: String::new(),
            sync_detected: Vec::new(),
            sync_detected_idx: 0,
            detect_job: None,
            center_request: None,
            label_search: String::new(),
            auto_map: true,
            manual_map_lock: false,
            pending_region: None,
            region_lock: None,
            pending_layer: None,
            layer_authoritative_at: None,
            active_layer: None,
            layers_open: false,
            learned_layers: Self::load_learned_layers(),
            learned_layers_dirty: false,
            sync_job: None,
            export_job: None,
            pos_history: Vec::new(),
            auto_notes: Vec::new(),
        }
    }

    /// Storage bucket for the active UID (0 before a UID is detected).
    fn bucket(&self) -> u32 {
        self.active_uid.unwrap_or(0)
    }

    /// Queue a packet-detected chest open for auto-matching against API pins.
    pub fn note_chest(&mut self, chest: ChestMark) {
        if self.pending_chests.len() >= 50 {
            self.pending_chests.remove(0);
        }
        self.pending_chests.push(chest);
    }

    /// Queue an oculus collection (player position when the oculus gadget
    /// config id appeared in a small command).
    pub fn note_oculus(&mut self, x: f32, z: f32) {
        if self.pending_oculi.len() >= 50 {
            self.pending_oculi.remove(0);
        }
        self.pending_oculi.push((x, z));
    }

    /// Queue a challenge completion detected via the 20234-shape success
    /// packet (player position at completion time).
    pub fn note_challenge_done(&mut self, x: f32, z: f32) {
        if self.pending_challenges.len() >= 50 {
            self.pending_challenges.remove(0);
        }
        self.pending_challenges.push((x, z));
    }

    /// Queue a teleport arrival for floor auto-selection. `dy` is the
    /// height delta of the jump — stacked-floor teleports are disambiguated
    /// by it (x-z alone can't tell floor N from floor N+1 above it).
    pub fn note_teleport(&mut self, x: f32, y: f32, z: f32, dy: f32) {
        if self.pending_teleports.len() >= 20 {
            self.pending_teleports.remove(0);
        }
        self.pending_teleports.push((x, y, z, dy));
    }

    /// Packet-driven region switch (6771 broadcast): authoritative —
    /// switches even without follow, when auto-map is enabled. While a
    /// special region is active, geometric auto-switching stands down.
    pub fn note_region(&mut self, map_id: u32) {
        self.region_lock = (map_id != 2).then_some(map_id);
        if self.auto_map && map_id != self.selected_map {
            tracing::info!("region broadcast → switching to map {map_id}");
            self.pending_region = Some(map_id);
        }
    }

    /// Minimap layer report (5991 _EnterMapLayerReq). `None` = the client
    /// returned to the default (base) layer — clear the floor. A named
    /// layer is applied from the learned mapping immediately, or queued
    /// for geometry-based learning when unknown.
    pub fn note_map_layer(&mut self, layer_id: Option<u64>) {
        match layer_id {
            None => {
                self.pending_layer = None;
                self.active_layer = None;
                self.active_floor = None;
                // Lock SURFACE: the default-layer report is authoritative
                // (the minimap switched outdoors). Without the lock the
                // rect auto-follow would immediately re-select the floor
                // we just left — the layer boundary sits INSIDE the
                // overlay rect, so containment still matches at exit.
                self.floor_locked = true;
                self.layer_authoritative_at =
                    Some(std::time::Instant::now());
            }
            Some(id) => {
                self.pending_layer = Some(id);
                // Track even before resolution so the mapping manager can
                // offer manual teaching for never-learned layers.
                self.active_layer = Some(id);
            }
        }
    }

    fn learned_layers_path() -> std::path::PathBuf {
        Self::data_dir().join("learned_map_layers.json")
    }

    /// Download (once) / load / cache a floor overlay texture. Returns the
    /// texture id when available.
    fn ensure_floor_overlay(
        overlays: &mut HashMap<u32, egui::TextureHandle>,
        floor: &pins::FloorInfo,
        ctx: &egui::Context,
    ) -> Option<egui::TextureId> {
        if !overlays.contains_key(&floor.floor_id) {
            let path = Self::data_dir().join("map").join(format!(
                "floor_{}.png",
                floor.floor_id
            ));
            if !path.exists() {
                let agent = ureq::AgentBuilder::new()
                    .user_agent("Mozilla/5.0")
                    .build();
                if let Ok(resp) = agent.get(&floor.overlay_url).call() {
                    use std::io::Read as _;
                    let mut buf = Vec::new();
                    let mut r = resp.into_reader();
                    if r.read_to_end(&mut buf).is_ok() {
                        let _ = std::fs::write(&path, &buf);
                    }
                }
            }
            if let Ok(img) = image::open(&path) {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let tex = ctx.load_texture(
                    format!("floor_{}", floor.floor_id),
                    egui::ColorImage::from_rgba_unmultiplied(
                        [w as usize, h as usize],
                        &rgba,
                    ),
                    egui::TextureOptions::LINEAR,
                );
                overlays.insert(floor.floor_id, tex);
            }
        }
        overlays.get(&floor.floor_id).map(|t| t.id())
    }

    fn load_learned_layers(
    ) -> std::collections::HashMap<
        u32,
        std::collections::HashMap<u64, (u32, u32)>,
    > {
        let Ok(text) = std::fs::read_to_string(Self::learned_layers_path())
        else {
            return std::collections::HashMap::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return std::collections::HashMap::new();
        };
        let mut out = std::collections::HashMap::new();
        if let Some(maps) = v.as_object() {
            for (map_str, entries) in maps {
                let Ok(map_id) = map_str.parse::<u32>() else { continue };
                let Some(obj) = entries.as_object() else { continue };
                let mut inner = std::collections::HashMap::new();
                for (id_str, pair) in obj {
                    let Ok(id) = id_str.parse::<u64>() else { continue };
                    if let Some(arr) = pair.as_array() {
                        if arr.len() == 2 {
                            if let (Some(g), Some(f)) =
                                (arr[0].as_u64(), arr[1].as_u64())
                            {
                                inner.insert(id, (g as u32, f as u32));
                            }
                        }
                    }
                }
                out.insert(map_id, inner);
            }
        }
        out
    }

    fn save_learned_layers(&self) {
        Self::write_learned_layers(&self.learned_layers);
    }

    fn write_learned_layers(
        map: &std::collections::HashMap<
            u32,
            std::collections::HashMap<u64, (u32, u32)>,
        >,
    ) {
        let mut v = serde_json::Map::new();
        for (map_id, entries) in map {
            let mut inner = serde_json::Map::new();
            for (id, (g, f)) in entries {
                inner.insert(id.to_string(), serde_json::json!([g, f]));
            }
            v.insert(
                map_id.to_string(),
                serde_json::Value::Object(inner),
            );
        }
        let _ = std::fs::write(
            Self::learned_layers_path(),
            serde_json::to_string_pretty(&serde_json::Value::Object(v))
                .unwrap_or_default(),
        );
    }

    fn data_dir() -> std::path::PathBuf {
        std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
            .unwrap_or_default().join("GenshinExplorer")
    }
    fn calibration_path(map_id: u32) -> Option<std::path::PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|v| std::path::PathBuf::from(v)
            .join("GenshinExplorer").join(format!("map_calibration_{map_id}.json")))
    }
    fn load_calibration(map_id: u32)
        -> (Option<(f64, f64)>, Option<(f64, f64)>, Vec<Vec<CalPoint>>)
    {
        let Ok(text) = std::fs::read_to_string(
            Self::calibration_path(map_id).unwrap_or_default(),
        ) else {
            return (None, None, Vec::new());
        };
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&text) else {
            return (None, None, Vec::new());
        };
        let g = |k: &str| j.get(k).and_then(|v| v.as_f64());
        let offset = match (g("offset_x"), g("offset_y")) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        };
        let scale = match (g("scale_x"), g("scale_y")) {
            (Some(x), Some(y)) if x > 0.05 && y > 0.05 => Some((x, y)),
            _ => None,
        };
        let parse_point = |p: &serde_json::Value| -> Option<CalPoint> {
            Some(CalPoint {
                wx: p.get("wx")?.as_f64()? as f32,
                wz: p.get("wz")?.as_f64()? as f32,
                mx: p.get("mx")?.as_f64()?,
                my: p.get("my")?.as_f64()?,
                scene_gen: p.get("scene_gen")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
            })
        };
        let mut frames: Vec<Vec<CalPoint>> = Vec::new();
        if let Some(arr) = j.get("points").and_then(|v| v.as_array()) {
            if let Some(first) = arr.first() {
                if first.get("points").is_some() {
                    // New format: [{points: [...]}, {points: [...]}]
                    for fr in arr {
                        if let Some(pts) =
                            fr.get("points").and_then(|v| v.as_array())
                        {
                            frames.push(
                                pts.iter().filter_map(|p| parse_point(p))
                                    .collect(),
                            );
                        }
                    }
                } else {
                    // Old format: flat array of points = one frame.
                    let f: Vec<CalPoint> =
                        arr.iter().filter_map(|p| parse_point(p)).collect();
                    if !f.is_empty() { frames.push(f); }
                }
            }
        }
        (offset, scale, frames)
    }
    fn save_calibration(
        map_id: u32,
        offset: Option<(f64, f64)>,
        scale: Option<(f64, f64)>,
        frames: &[Vec<CalPoint>],
    ) {
        if let Some(p) = Self::calibration_path(map_id) {
            let mut v = serde_json::json!({});
            if let Some((x, y)) = offset {
                v["offset_x"] = x.into();
                v["offset_y"] = y.into();
            }
            if let Some((x, y)) = scale {
                v["scale_x"] = x.into();
                v["scale_y"] = y.into();
            }
            let frames_json: Vec<_> = frames
                .iter()
                .map(|f| serde_json::json!({
                    "points": serde_json::to_value(f).unwrap_or_default(),
                }))
                .collect();
            if !frames_json.is_empty() {
                v["points"] = frames_json.into();
            }
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, v.to_string());
        }
    }

    fn completed_path() -> Option<std::path::PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|v| std::path::PathBuf::from(v)
            .join("GenshinExplorer").join("completed_pins.json"))
    }
    fn load_completed() -> HashMap<(u32, u32), std::collections::HashSet<u64>> {
        let Ok(text) = std::fs::read_to_string(Self::completed_path().unwrap_or_default()) else {
            return HashMap::new();
        };
        let Ok(v) = serde_json::from_str::<HashMap<String, Vec<u64>>>(&text) else {
            return HashMap::new();
        };
        // Keys are "uid_mapid"; legacy files used bare "mapid" (uid 0).
        v.into_iter().filter_map(|(k, ids)| {
            let key = if let Some((u, m)) = k.split_once('_') {
                Some((u.parse().ok()?, m.parse().ok()?))
            } else {
                Some((0u32, k.parse().ok()?))
            };
            key.map(|k| (k, ids.into_iter().collect()))
        }).collect()
    }
    fn save_completed(&self) {
        Self::write_completed(&self.completed);
    }
    fn write_completed(completed: &HashMap<(u32, u32), std::collections::HashSet<u64>>) {
        if let Some(p) = Self::completed_path() {
            let map: HashMap<String, Vec<u64>> = completed.iter()
                .map(|((u, m), set)|
                    (format!("{u}_{m}"), set.iter().copied().collect()))
                .collect();
            let _ = std::fs::write(p, serde_json::to_string(&map).unwrap_or_default());
        }
    }

    /// Effective UID for sync operations: the selected profile's uid, else
    /// the picked detected entry's uid, else the active game uid.
    fn sync_uid(&self) -> Option<u32> {
        self.sync_profile_idx
            .and_then(|i| self.sync_profiles.get(i))
            .and_then(|p| p.uid)
            .or_else(|| self.sync_detected.get(self.sync_detected_idx).and_then(|d| d.uid))
            .or(self.active_uid)
    }

    fn start_pin_load(&mut self) {
        if !matches!(self.pin_state, PinState::NotLoaded) { return; }
        let map_id = self.selected_map;
        let result: Arc<Mutex<Option<anyhow::Result<PinData>>>> = Arc::new(Mutex::new(None));
        let r2 = result.clone();
        let dir = Self::data_dir().join("map");
        std::thread::spawn(move || {
            *r2.lock().unwrap() = Some(pins::load_pin_data(map_id, &dir));
        });
        self.pin_state = PinState::Loading(result);
    }

    fn start_map_download(&mut self, cache_dir: &std::path::Path) {
        let map_id = self.selected_map;
        let done = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(1));
        let result: Arc<Mutex<Option<anyhow::Result<MapData>>>> = Arc::new(Mutex::new(None));
        let (d2, t2, r2) = (done.clone(), total.clone(), result.clone());
        let dir = cache_dir.to_path_buf();
        std::thread::spawn(move || {
            *r2.lock().unwrap() = Some(map::ensure_map(&dir, map_id, &|d, t| {
                d2.store(d, Ordering::Relaxed);
                t2.store(t, Ordering::Relaxed);
            }));
        });
        self.state = MapLoadState::Downloading { done, total, result };
    }

    /// Renders the map docked into the given panel ui (resizes with it).
    pub fn show_docked(&mut self, ui: &mut egui::Ui,
                       player: Option<(f32,f32,f32)>, cache_dir: &std::path::Path,
                       uid: Option<u32>) {
        // UID switch → swap filter/completion buckets (per-account state).
        if uid != self.active_uid {
            let old_bucket = self.bucket();
            tracing::info!(from = old_bucket, to = uid.unwrap_or(0), "switching UID bucket");
            self.pin_filter.save(&Self::data_dir(), old_bucket, self.selected_map);
            self.active_uid = uid;
            self.pin_filter = pins::FilterState::load(
                &Self::data_dir(), self.bucket(), self.selected_map);
            if self.pin_filter.enabled_labels.is_empty() {
                if let PinState::Loaded(d) = &self.pin_state {
                    self.pin_filter = pins::FilterState::with_defaults(d);
                }
            }
        }

        // Prune stale toasts.
        self.auto_notes.retain(|(_, t)| t.elapsed().as_secs_f32() < 4.5);

        // Maintain position history (~2s cadence) for challenge dwell
        // detection.
        if let Some((x, _, z)) = player {
            let due = self
                .pos_history
                .last()
                .map(|(_, _, t)| t.elapsed().as_secs_f32() >= 2.0)
                .unwrap_or(true);
            if due {
                self.pos_history.push((x, z, std::time::Instant::now()));
                if self.pos_history.len() > 900 {
                    let drop = self.pos_history.len() - 900;
                    self.pos_history.drain(0..drop);
                }
            }
        }

        // Poll tile downloads → textures.
        if let Some(store) = self.tiles.as_mut() {
            store.poll(ui.ctx());
        }

        // Handle pin loading
        let pin_result = if let PinState::Loading(r) = &self.pin_state {
            r.lock().unwrap().take()
        } else { None };
        if let Some(r) = pin_result {
            match r {
                Ok(data) => {
                    if self.pin_filter.enabled_labels.is_empty() {
                        self.pin_filter = pins::FilterState::with_defaults(&data);
                    }
                    self.icons_initialized = false;
                    self.pin_state = PinState::Loaded(Arc::new(data));
                }
                Err(e) => { tracing::warn!("pins failed: {e:#}"); self.pin_state = PinState::NotLoaded; }
            }
        }
        if matches!(self.state, MapLoadState::Ready{..}) && matches!(self.pin_state, PinState::NotLoaded) {
            self.start_pin_load();
        }

        let mut next_state = None;
        let mut filter_changed = false;
        let mut pan = self.pan; let mut zoom = self.zoom;
        let mut follow = self.follow; let mut calibrating = self.calibrating;
        let mut view_init = self.view_init;
        let mut new_map: Option<u32> = None;
        let mut manual_lock = self.manual_map_lock;

        let pin_data = if let PinState::Loaded(d) = &self.pin_state { Some(d.clone()) } else { None };
        let mut pin_filter = self.pin_filter.clone();
        let selected_map = self.selected_map;
        let bucket = self.bucket();
        let completed = self.completed.get(&(bucket, selected_map)).cloned()
            .unwrap_or_default();
        let mut selected = self.selected.clone();

        // ── HoYoLab import result ──
        if let Some(job) = self.sync_job.clone() {
            let result = job.lock().unwrap().take();
            if let Some(res) = result {
                self.sync_job = None;
                match res {
                    Ok(marks) => {
                        match &pin_data {
                            Some(pd) => {
                                // Marks land in the sync uid's bucket
                                // (profile uid or the active game uid).
                                let sync_bucket =
                                    self.sync_uid().unwrap_or(bucket);
                                let set = self.completed
                                    .entry((sync_bucket, selected_map)).or_default();
                                let mut n = 0;
                                for id in &marks.point_ids {
                                    if pd.pin_label.contains_key(id) && set.insert(*id) {
                                        n += 1;
                                    }
                                }
                                Self::write_completed(&self.completed);
                                self.sync_status = Some(format!(
                                    "⬇ Imported {n} new pins ({} marks total)",
                                    marks.total,
                                ));
                                self.auto_notes.push((
                                    format!("⬇ Imported {n} pins from HoYoLab"),
                                    std::time::Instant::now(),
                                ));
                            }
                            None => {
                                self.sync_status = Some(
                                    "✗ Pins not loaded yet — open the map first".into(),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        self.sync_status = Some(format!("✗ {e:#}"));
                    }
                }
            }
        }

        match &mut self.state {
            MapLoadState::Idle => {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.heading("Select Map");
                    new_map = Self::map_selector(ui, "map_select", selected_map);
                    ui.add_space(12.0);
                    if ui.button("📥 Download Map & Pins").clicked() {
                        self.start_pin_load();
                        self.start_map_download(cache_dir);
                    }
                });
            }
            MapLoadState::Downloading{done,total,result} => {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    let d = done.load(Ordering::Relaxed);
                    let t = total.load(Ordering::Relaxed).max(1);
                    ui.label(format!("Downloading overview tiles… {d}/{t}"));
                    ui.add(egui::ProgressBar::new(d as f32 / t as f32).show_percentage());
                    if let Some(r) = result.lock().unwrap().take() {
                        match r {
                            Ok(data) => next_state =
                                Some(MapLoadState::Ready{texture:None,data:Arc::new(data)}),
                            Err(e) => {
                                ui.colored_label(egui::Color32::RED, format!("{e:#}"));
                                next_state = Some(MapLoadState::Idle);
                            }
                        }
                    }
                });
            }
            MapLoadState::Ready{texture,data} => {
                if texture.is_none() {
                    *texture = Some(ui.ctx().load_texture(
                        format!("map_overview_{}", data.map_id),
                        data.color_image.clone(), egui::TextureOptions::LINEAR));
                }
                if let Some(tex) = texture.as_ref() {
                    let md = data.clone();

                    // Lazily create the tile store for this cache dir.
                    if self.tiles.is_none() {
                        self.tiles = Some(TileStore::new(cache_dir.to_path_buf()));
                    }
                    let store = self.tiles.as_mut().unwrap();

                    if !self.icons_initialized {
                        if let Some(pd) = &pin_data {
                            for label in &pd.labels {
                                if let Some(img) = &label.icon_image {
                                    let t = ui.ctx().load_texture(
                                        format!("icon_{}", label.id),
                                        img.clone(), egui::TextureOptions::LINEAR);
                                    self.icon_textures.insert(label.id, t);
                                }
                            }
                            self.icons_initialized = true;
                        }
                    }
                    let icon_textures = self.icon_textures.clone();

                    // Auto-collect: challenge completed (20234-shape success)
                    // — the player stands at the challenge, so match the
                    // nearest uncollected challenge pin directly.
                    if !self.pending_challenges.is_empty() {
                        if let Some(pd) = pin_data.as_ref() {
                            let bucket = self.active_uid.unwrap_or(0);
                            let xf = {
                                let (sx, sy) = self.cal_scale
                                    .unwrap_or((md.world_scale, md.world_scale));
                                let (ox, oy) =
                                    self.calibrate_offset.unwrap_or((0.0, 0.0));
                                Xform { sx, sy, ox, oy }
                            };
                            let ch_ids = pd.semantic_labels(PinCategory::Challenges);
                            let events = std::mem::take(&mut self.pending_challenges);
                            for (x, z) in &events {
                                if let Some(name) = Self::match_pin(
                                    &md, pd, &mut self.completed, bucket, xf,
                                    *x, *z, 80.0, &ch_ids, None,
                                ) {
                                    tracing::info!("challenge completed: {name}");
                                    let (mx, my) = xf.apply(md.origin, *x, *z);
                                    self.recent_challenge_done.push((
                                        mx, my, std::time::Instant::now(),
                                    ));
                                    self.auto_notes.push((
                                        format!("🏁 {name} completed"),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                            self.recent_challenge_done.retain(|(_, _, t)| {
                                t.elapsed().as_secs_f32() < 600.0
                            });
                            Self::write_completed(&self.completed);
                        }
                    }

                    // Auto-collect: match packet-detected chest opens to
                    // nearby un-collected API chest pins.
                    //
                    // Challenges (time trials etc.) pin the START, but the
                    // reward chest spawns elsewhere. Dwell rule first: if the
                    // player stood at an uncollected challenge pin (≥6s within
                    // the last 5min) and the chest event is within 250px of
                    // it, mark the CHALLENGE — spawned chests often sit near
                    // unrelated chest pins, so the dwell anchor wins.
                    // Chests opening near a JUST-completed challenge (instant
                    // path above) are its spawned reward — skip them entirely.
                    if !self.pending_chests.is_empty() {
                        if let Some(pd) = pin_data.as_ref() {
                            let bucket = self.active_uid.unwrap_or(0);
                            let xf = {
                                let (sx, sy) = self.cal_scale
                                    .unwrap_or((md.world_scale, md.world_scale));
                                let (ox, oy) =
                                    self.calibrate_offset.unwrap_or((0.0, 0.0));
                                Xform { sx, sy, ox, oy }
                            };
                            let events = std::mem::take(&mut self.pending_chests);
                            for ev in &events {
                                let (cx, cy) = xf.apply(md.origin, ev.x, ev.z);
                                let near_recent_challenge = self.recent_challenge_done
                                    .iter()
                                    .any(|(rx, ry, _)| {
                                        ((rx - cx).powi(2) + (ry - cy).powi(2)).sqrt() <= 250.0
                                    });
                                let note = if near_recent_challenge {
                                    tracing::debug!("chest event near recent challenge — spawned reward, skipping");
                                    None
                                } else {
                                    Self::match_chest_or_challenge(
                                        &md, pd, &mut self.completed, bucket, xf,
                                        ev, &self.pos_history,
                                    )
                                };
                                if let Some(note) = note {
                                    self.auto_notes.push((note, std::time::Instant::now()));
                                }
                            }
                            Self::write_completed(&self.completed);
                        }
                        // Pins not loaded yet → events stay queued.
                    }

                    // Auto-collect: oculus gadget state change + player
                    // standing at an uncollected oculus pin.
                    if !self.pending_oculi.is_empty() {
                        if let Some(pd) = pin_data.as_ref() {
                            let bucket = self.active_uid.unwrap_or(0);
                            let xf = {
                                let (sx, sy) = self.cal_scale
                                    .unwrap_or((md.world_scale, md.world_scale));
                                let (ox, oy) =
                                    self.calibrate_offset.unwrap_or((0.0, 0.0));
                                Xform { sx, sy, ox, oy }
                            };
                            let oc_ids = pd.semantic_labels(PinCategory::Oculi);
                            let events = std::mem::take(&mut self.pending_oculi);
                            for (x, z) in &events {
                                if let Some(name) = Self::match_pin(
                                    &md, pd, &mut self.completed, bucket, xf,
                                    *x, *z, 45.0, &oc_ids, None,
                                ) {
                                    tracing::info!("auto-collected: {name}");
                                    self.auto_notes.push((
                                        format!("✓ {name} auto-collected"),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                            if self.auto_notes.len() > 8 {
                                let drop = self.auto_notes.len() - 8;
                                self.auto_notes.drain(0..drop);
                            }
                            Self::write_completed(&self.completed);
                        }
                    }

                    // ── Sidebar ──
                    let mut sync_open = self.sync_open;
                    let mut sync_cookie = self.sync_cookie.clone();
                    let mut sync_auto = self.sync_auto;
                    let mut sync_status = self.sync_status.clone();
                    let mut profiles = self.sync_profiles.clone();
                    let mut sync_profile_idx = self.sync_profile_idx;
                    let mut sync_profile_name = self.sync_profile_name.clone();
                    let mut sync_profile_uid = self.sync_profile_uid.clone();
                    let mut sync_detected = self.sync_detected.clone();
                    let mut sync_detected_idx = self.sync_detected_idx;
                    let mut detect_job = self.detect_job.clone();
                    if let Some((prog, _)) = &self.export_job {
                        // Live export progress overrides the static status.
                        sync_status = Some(prog.lock().unwrap().clone());
                    }
                    let sync_busy = self.sync_job.is_some();
                    let export_job = self.export_job.clone();
                    let mut start_sync = false;
                    let mut start_export = false;
                    let mut bulk_action: Option<(u32, bool)> = None;
                    let mut label_search = std::mem::take(&mut self.label_search);
                    let mut floor_select: Option<Option<usize>> = None;
                    let mut floor_pick = self.active_floor;
                    // Layer-mapping manager state (locals to keep the
                    // closures free of `self` borrows).
                    let mut layers_open = self.layers_open;
                    let mut learned = std::mem::take(&mut self.learned_layers);
                    let mut learned_dirty = self.learned_layers_dirty;
                    let cur_layer = self.active_layer;
                    egui::SidePanel::left("map_filters")
                        .resizable(true).default_width(230.0).min_width(200.0)
                        .show_inside(ui, |ui| {
                            // Bottom-fixed section: summary + HoYoLab sync —
                            // always visible regardless of scroll content.
                            egui::TopBottomPanel::bottom("map_sync")
                                .show_inside(ui, |ui| {
                                    if let Some(pd) = &pin_data {
                                        let visible = pd.pins.iter()
                                            .filter(|p| pin_filter.is_enabled(p.label_id)).count();
                                        let done_visible = pd.pins.iter()
                                            .filter(|p| pin_filter.is_enabled(p.label_id)
                                                && completed.contains(&p.id)).count();
                                        ui.weak(format!("{visible} pins enabled · {done_visible} collected"));
                                    }
                                    ui.separator();
                                    let busy = sync_busy || export_job.is_some()
                                        || detect_job.is_some();
                                    ui.horizontal(|ui| {
                                        if ui.add_enabled(!busy, egui::Button::new("⬇ Import"))
                                            .clicked()
                                        {
                                            sync_open = !sync_open;
                                        }
                                        if ui.add_enabled(!busy, egui::Button::new("⬆ Export"))
                                            .clicked()
                                        {
                                            start_export = true;
                                        }
                                        if ui.add_enabled(!busy, egui::Button::new("🧲 Auto"))
                                            .on_hover_text(
                                                "Detect HoYoLab cookies — Firefox (incl. containers), Edge/Chrome profiles — with per-region game characters")
                                            .clicked()
                                            && detect_job.is_none()
                                        {
                                            let job: Arc<Mutex<Option<Vec<cookies::FoundCookie>>>> =
                                                Arc::new(Mutex::new(None));
                                            let j2 = job.clone();
                                            std::thread::spawn(move || {
                                                // Scan browsers, then expand each
                                                // session by its game characters
                                                // (one per region).
                                                let mut entries = Vec::new();
                                                for session in cookies::auto_detect_all() {
                                                    let roles =
                                                        sync::fetch_game_roles(&session.cookie);
                                                    if roles.is_empty() {
                                                        entries.push(session);
                                                    } else {
                                                        for r in roles {
                                                            entries.push(cookies::FoundCookie {
                                                                cookie: session.cookie.clone(),
                                                                source: format!(
                                                                    "{} — {} ({} {})",
                                                                    session.source, r.nickname,
                                                                    sync::region_display(&r.region),
                                                                    r.uid,
                                                                ),
                                                                uid: Some(r.uid),
                                                            });
                                                        }
                                                    }
                                                }
                                                *j2.lock().unwrap() = Some(entries);
                                            });
                                            detect_job = Some(job);
                                            sync_status = Some("🧲 Scanning browsers…".into());
                                        }
                                    });
                                    // Profile selector.
                                    ui.horizontal(|ui| {
                                        ui.label("Profile:");
                                        let mut sel = sync_profile_idx;
                                        let label = sel
                                            .and_then(|i| profiles.get(i))
                                            .map(|p| p.name.clone())
                                            .unwrap_or_else(|| "— session —".into());
                                        egui::ComboBox::from_id_salt("sync_profile")
                                            .selected_text(label)
                                            .width(ui.available_width() - 58.0)
                                            .show_ui(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut sel, None, "— session —");
                                                for (i, p) in profiles.iter().enumerate() {
                                                    ui.selectable_value(
                                                        &mut sel, Some(i),
                                                        format!("{}{}", p.name,
                                                            p.uid.map(|u| format!(" ({u})"))
                                                                .unwrap_or_default()));
                                                }
                                            });
                                        if sel != sync_profile_idx {
                                            sync_profile_idx = sel;
                                            if let Some(i) = sel {
                                                if let Some(p) = profiles.get(i) {
                                                    sync_cookie = p.cookie.clone();
                                                    sync_status =
                                                        Some(format!("✓ using {}", p.name));
                                                }
                                            }
                                        }
                                        if sync_profile_idx.is_some()
                                            && ui.button("🗑").clicked()
                                        {
                                            if let Some(i) = sync_profile_idx.take() {
                                                if i < profiles.len() {
                                                    let name = profiles[i].name.clone();
                                                    profiles.remove(i);
                                                    if cookies::save_profiles(&profiles).is_ok() {
                                                        sync_status =
                                                            Some(format!("🗑 removed {name}"));
                                                    }
                                                }
                                            }
                                        }
                                    });
                                    if busy { ui.spinner(); }
                                    if let Some(st) = &sync_status { ui.small(st); }
                                    if sync_open && !busy {
                                        // Detected-session picker (multi-account /
                                        // containers). Selection tracked by index —
                                        // entries from one login share a cookie.
                                        if sync_detected.len() > 1 {
                                            ui.horizontal(|ui| {
                                                ui.label("Detected:");
                                                let sources: Vec<&str> = sync_detected
                                                    .iter().map(|d| d.source.as_str()).collect();
                                                let mut pick =
                                                    sync_detected_idx.min(sources.len() - 1);
                                                egui::ComboBox::from_id_salt("detected")
                                                    .selected_text(sources[pick].to_string())
                                                    .width(160.0)
                                                    .show_ui(ui, |ui| {
                                                        for (i, s) in sources.iter().enumerate() {
                                                            ui.selectable_value(
                                                                &mut pick, i, *s);
                                                        }
                                                    });
                                                if pick != sync_detected_idx {
                                                    sync_detected_idx = pick;
                                                    sync_cookie =
                                                        sync_detected[pick].cookie.clone();
                                                    if let Some(uid) =
                                                        sync_detected[pick].uid
                                                    {
                                                        sync_profile_uid = uid.to_string();
                                                    }
                                                }
                                            });
                                        }
                                        ui.horizontal(|ui| {
                                            ui.add(egui::TextEdit::singleline(
                                                    &mut sync_profile_name)
                                                .desired_width(80.0)
                                                .hint_text("name"));
                                            ui.add(egui::TextEdit::singleline(
                                                    &mut sync_profile_uid)
                                                .desired_width(70.0)
                                                .hint_text("game UID (opt)"));
                                            if ui.button("💾 Save profile").clicked() {
                                                let name = sync_profile_name.trim().to_string();
                                                if name.is_empty() {
                                                    sync_status = Some(
                                                        "✗ enter a profile name".into());
                                                } else if !sync_cookie.contains("cookie_token_v2") {
                                                    sync_status = Some(
                                                        "✗ no cookie to save (🧲 or paste first)".into());
                                                } else {
                                                    let uid = sync_profile_uid.trim()
                                                        .parse::<u32>().ok();
                                                    let profile = cookies::CookieProfile {
                                                        name: name.clone(),
                                                        uid,
                                                        cookie: sync_cookie.clone(),
                                                    };
                                                    // Upsert by name.
                                                    if let Some(i) = profiles.iter()
                                                        .position(|p| p.name == name)
                                                    {
                                                        profiles[i] = profile;
                                                    } else {
                                                        profiles.push(profile);
                                                    }
                                                    sync_profile_idx =
                                                        profiles.iter().position(
                                                            |p| p.name == name);
                                                    match cookies::save_profiles(&profiles) {
                                                        Ok(()) => sync_status = Some(
                                                            format!("💾 saved {name}")),
                                                        Err(e) => sync_status = Some(
                                                            format!("✗ save failed: {e:#}")),
                                                    }
                                                }
                                            }
                                        });
                                        ui.add(egui::TextEdit::singleline(&mut sync_cookie)
                                            .desired_width(180.0)
                                            .hint_text("F12 → Network → Cookie header"));
                                        ui.horizontal(|ui| {
                                            if ui.button("Fetch marks").clicked() {
                                                start_sync = true;
                                                sync_open = false;
                                            }
                                            if ui.button("Close").clicked() {
                                                sync_open = false;
                                            }
                                        });
                                    }
                                    ui.checkbox(&mut sync_auto, "Auto-sync manual marks")
                                        .on_hover_text(
                                            "Manual Mark/Undo in the pin popup also pushes to HoYoLab \
                                             (add on mark, delete on undo). Packet-detected \
                                             auto-collections still need ⬆ Export.");
                                });
                            // Scrollable top section.
                            // Manual map pick → lock (region packets
                            // still override).
                            let mut manual_lock = self.manual_map_lock;
                            let manual_pick = Self::map_selector(
                                ui,
                                "map_sel_sidebar",
                                selected_map,
                            );
                            if manual_pick.is_some() {
                                new_map = manual_pick;
                                manual_lock = true;
                            }
                            if manual_lock {
                                ui.label("🔒")
                                    .on_hover_text(
                                        "Manual map selection — geometric \
                                         auto-switching paused. A region \
                                         packet (Chasm / moon / surface) \
                                         or cycling 🗺 auto resumes it.",
                                    );
                            }
                            // Floor (layer) selector — only on layered maps.
                            if let Some(pd) = pin_data.as_ref() {
                                if !pd.floors.is_empty() {
                                    let cur = floor_pick
                                        .and_then(|i| pd.floors.get(i));
                                    let label = cur
                                        .map(|f| format!("{}: {}", f.group_name, f.name))
                                        .unwrap_or_else(|| "Surface".into());
                                    egui::ComboBox::from_id_salt("floor_sel")
                                        .selected_text(label)
                                        .width(200.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut floor_pick, None, "Surface");
                                            for (i, f) in
                                                pd.floors.iter().enumerate()
                                            {
                                                ui.selectable_value(
                                                    &mut floor_pick, Some(i),
                                                    format!("{}: {}",
                                                        f.group_name, f.name));
                                            }
                                        });
                                    if floor_pick != self.active_floor {
                                        floor_select = Some(floor_pick);
                                    }
                                    if ui.button("🗂")
                                        .on_hover_text(
                                            "Learned minimap-layer → floor mappings: \
                                             fix a wrong entry or forget one (it will \
                                             re-learn on your next visit).")
                                        .clicked()
                                    {
                                        layers_open = true;
                                    }
                                }
                            }
                            ui.separator();
                            match &pin_data {
                                None => { ui.spinner(); ui.label("Loading pins…"); }
                                Some(pd) => Self::sidebar(ui, pd, &mut pin_filter,
                                    &icon_textures, &mut filter_changed, &completed,
                                    &mut bulk_action, &mut label_search),
                            }
                        });
                    self.sync_open = sync_open;
                    self.sync_cookie = sync_cookie;
                    self.sync_auto = sync_auto;
                    self.manual_map_lock = manual_lock;
                    self.sync_profiles = profiles;
                    self.sync_profile_idx = sync_profile_idx;
                    self.sync_profile_name = sync_profile_name;
                    self.sync_profile_uid = sync_profile_uid;
                    self.sync_detected = sync_detected;
                    self.sync_detected_idx = sync_detected_idx;
                    self.detect_job = detect_job;
                    self.layers_open = layers_open;
                    self.learned_layers = learned;
                    self.learned_layers_dirty = learned_dirty;

                    // ── Layer-mapping manager window ──
                    if self.layers_open {
                        let mut layers_open = self.layers_open;
                        let mut learned = std::mem::take(&mut self.learned_layers);
                        let mut learned_dirty = self.learned_layers_dirty;
                        let cur_layer = self.active_layer;
                        let cur_floor = self.active_floor;
                        // Some(None) = apply surface, Some(Some(i)) = floor.
                        let mut apply_active: Option<Option<usize>> = None;
                        let pd = pin_data.clone();
                        egui::Window::new("🗂 Layer mappings")
                            .open(&mut layers_open)
                            .resizable(true)
                            .default_width(460.0)
                            .show(ui.ctx(), |ui| {
                                let Some(pd) = pd.as_deref() else {
                                    ui.label("Pins not loaded yet.");
                                    return;
                                };
                                ui.label(format!(
                                    "Minimap-layer → floor (map {selected_map})"));
                                ui.small(format!(
                                    "active layer: {}",
                                    cur_layer.map(|l| l.to_string())
                                        .unwrap_or_else(|| "—".into())));
                                ui.small(
                                    "Entries are learned automatically as you \
                                     play. Fix a wrong floor here, or ✕ to \
                                     re-learn it on your next visit. Surface = \
                                     outdoors (no floor overlay).",
                                );
                                ui.add_space(4.0);

                                // The active layer has no entry yet — offer
                                // manual teaching (Surface or any floor).
                                if let Some(id) = cur_layer {
                                    let already = learned
                                        .get(&selected_map)
                                        .map(|m| m.contains_key(&id))
                                        .unwrap_or(false);
                                    if !already {
                                        let mut teach: Option<(u32, u32)> =
                                            None;
                                        ui.horizontal(|ui| {
                                            ui.label("📍");
                                            ui.monospace(id.to_string());
                                            ui.weak("(not learned — teach it:");
                                            egui::ComboBox::from_id_salt(
                                                format!("layer_teach_{id}"),
                                            )
                                            .selected_text("pick…")
                                            .width(250.0)
                                            .show_ui(ui, |ui| {
                                                if ui
                                                    .selectable_label(
                                                        false,
                                                        "Surface (no floor)",
                                                    )
                                                    .clicked()
                                                {
                                                    teach = Some((0, 0));
                                                }
                                                for fl in &pd.floors {
                                                    if ui
                                                        .selectable_label(
                                                            false,
                                                            format!(
                                                                "{}: {}",
                                                                fl.group_name,
                                                                fl.name
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        teach = Some((
                                                            fl.group_id,
                                                            fl.floor_id,
                                                        ));
                                                    }
                                                }
                                            });
                                            ui.weak(")");
                                        });
                                        if let Some((ng, nf)) = teach {
                                            learned
                                                .entry(selected_map)
                                                .or_default()
                                                .insert(id, (ng, nf));
                                            learned_dirty = true;
                                            apply_active = Some(
                                                if (ng, nf) == (0, 0) {
                                                    None
                                                } else {
                                                    pd.floors.iter().position(
                                                        |fl| {
                                                            fl.group_id == ng
                                                                && fl.floor_id
                                                                    == nf
                                                        },
                                                    )
                                                },
                                            );
                                        }
                                    }
                                }

                                // Inferred sibling layers: same stack
                                // prefix, unlearned — mapped to the
                                // previous floors of the active floor's
                                // group (API floors are ordered
                                // shallow→deep). Picking one teaches it.
                                let mut inferred: Vec<(u64, usize)> =
                                    Vec::new();
                                if let (Some(lid), Some(fi)) =
                                    (cur_layer, cur_floor)
                                {
                                    if fi < pd.floors.len() {
                                        let floor = &pd.floors[fi];
                                        let group_floors: Vec<usize> = pd
                                            .floors
                                            .iter()
                                            .enumerate()
                                            .filter(|(_, f)| {
                                                f.group_id == floor.group_id
                                            })
                                            .map(|(i, _)| i)
                                            .collect();
                                        if let Some(pos) = group_floors
                                            .iter()
                                            .position(|i| *i == fi)
                                        {
                                            let prefix = lid / 100;
                                            let depth = (lid % 100).max(1)
                                                as usize;
                                            for d in 1..depth {
                                                let sid = prefix * 100 + d as u64;
                                                let already = learned
                                                    .get(&selected_map)
                                                    .map(|m| {
                                                        m.contains_key(&sid)
                                                    })
                                                    .unwrap_or(false);
                                                if already {
                                                    continue;
                                                }
                                                let off = depth - d;
                                                if pos >= off {
                                                    if let Some(&tfi) =
                                                        group_floors
                                                            .get(pos - off)
                                                    {
                                                        inferred
                                                            .push((sid, tfi));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if !inferred.is_empty() {
                                    ui.separator();
                                    ui.weak(
                                        "~ inferred siblings (unvisited, \
                                         guessed from floor-group order):",
                                    );
                                    for (sid, tfi) in inferred {
                                        let fl = &pd.floors[tfi];
                                        let mut pick: Option<(u32, u32)> =
                                            None;
                                        ui.horizontal(|ui| {
                                            ui.label("~");
                                            ui.monospace(sid.to_string());
                                            egui::ComboBox::from_id_salt(
                                                format!("layer_inf_{sid}"),
                                            )
                                            .selected_text(format!(
                                                "{}: {}",
                                                fl.group_name, fl.name
                                            ))
                                            .width(250.0)
                                            .show_ui(ui, |ui| {
                                                if ui
                                                    .selectable_label(
                                                        true,
                                                        format!(
                                                            "{}: {}",
                                                            fl.group_name,
                                                            fl.name
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    pick = Some((
                                                        fl.group_id,
                                                        fl.floor_id,
                                                    ));
                                                }
                                                if ui
                                                    .selectable_label(
                                                        false,
                                                        "Surface (no floor)",
                                                    )
                                                    .clicked()
                                                {
                                                    pick = Some((0, 0));
                                                }
                                                for f2 in &pd.floors {
                                                    if f2.group_id
                                                        == fl.group_id
                                                        && f2.floor_id
                                                            == fl.floor_id
                                                    {
                                                        continue;
                                                    }
                                                    if ui
                                                        .selectable_label(
                                                            false,
                                                            format!(
                                                                "{}: {}",
                                                                f2.group_name,
                                                                f2.name
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        pick = Some((
                                                            f2.group_id,
                                                            f2.floor_id,
                                                        ));
                                                    }
                                                }
                                            });
                                        });
                                        if let Some((ng, nf)) = pick {
                                            learned
                                                .entry(selected_map)
                                                .or_default()
                                                .insert(sid, (ng, nf));
                                            learned_dirty = true;
                                        }
                                    }
                                }

                                let entries: Vec<u64> = learned
                                    .get(&selected_map)
                                    .map(|m| {
                                        let mut ks: Vec<u64> =
                                            m.keys().copied().collect();
                                        ks.sort_unstable();
                                        ks
                                    })
                                    .unwrap_or_default();
                                if entries.is_empty() {
                                    ui.weak("(nothing learned on this map yet)");
                                }
                                egui::ScrollArea::vertical()
                                    .max_height(340.0)
                                    .show(ui, |ui| {
                                        for id in entries {
                                            let (g, f) = *learned
                                                .get(&selected_map)
                                                .and_then(|m| m.get(&id))
                                                .unwrap_or(&(0, 0));
                                            let is_surface = g == 0 && f == 0;
                                            let cur_text = if is_surface {
                                                "Surface (no floor)".to_owned()
                                            } else {
                                            pd.floors
                                                .iter()
                                                .find(|fl| {
                                                    fl.group_id == g
                                                        && fl.floor_id == f
                                                })
                                                .map(|fl| {
                                                    format!(
                                                        "{}: {}",
                                                        fl.group_name, fl.name
                                                    )
                                                })
                                                .unwrap_or_else(|| {
                                                    format!(
                                                        "group {g} / floor {f} \
                         (missing from API)"
                                                    )
                                                })
                                            };
                                            let mut pick: Option<(u32, u32)> =
                                                None;
                                            let mut del = false;
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    if cur_layer == Some(id) {
                                                        "📍"
                                                    } else {
                                                        "  "
                                                    },
                                                );
                                                ui.monospace(id.to_string());
                                                egui::ComboBox::from_id_salt(
                                                    format!("layer_fix_{id}"),
                                                )
                                                .selected_text(cur_text)
                                                .width(250.0)
                                                .show_ui(ui, |ui| {
                                                    if ui
                                                        .selectable_label(
                                                            is_surface,
                                                            "Surface (no floor)",
                                                        )
                                                        .clicked()
                                                    {
                                                        pick = Some((0, 0));
                                                    }
                                                    for fl in &pd.floors {
                                                        let selected =
                                                            fl.group_id == g
                                                                && fl.floor_id
                                                                    == f;
                                                        if ui
                                                            .selectable_label(
                                                                selected,
                                                                format!(
                                                                    "{}: {}",
                                                                    fl.group_name,
                                                                    fl.name
                                                                ),
                                                            )
                                                            .clicked()
                                                        {
                                                            pick = Some((
                                                                fl.group_id,
                                                                fl.floor_id,
                                                            ));
                                                        }
                                                    }
                                                });
                                                if ui
                                                    .button("✕")
                                                    .on_hover_text(
                                                        "forget this layer",
                                                    )
                                                    .clicked()
                                                {
                                                    del = true;
                                                }
                                            });
                                            if let Some((ng, nf)) = pick {
                                                if let Some(m) = learned
                                                    .get_mut(&selected_map)
                                                {
                                                    m.insert(id, (ng, nf));
                                                }
                                                learned_dirty = true;
                                                if cur_layer == Some(id) {
                                                    apply_active = Some(
                                                        if (ng, nf) == (0, 0) {
                                                            None
                                                        } else {
                                                            pd.floors
                                                                .iter()
                                                                .position(
                                                                    |fl| {
                                                                        fl.group_id == ng
                                                                && fl.floor_id
                                                                    == nf
                                                                    },
                                                                )
                                                        },
                                                    );
                                                }
                                            }
                                            if del {
                                                if let Some(m) = learned
                                                    .get_mut(&selected_map)
                                                {
                                                    m.remove(&id);
                                                }
                                                learned_dirty = true;
                                            }
                                        }
                                    });
                                ui.separator();
                                if ui
                                    .button("🗑 Forget all on this map")
                                    .clicked()
                                {
                                    learned.remove(&selected_map);
                                    learned_dirty = true;
                                }
                            });
                        self.layers_open = layers_open;
                        self.learned_layers = learned;
                        self.learned_layers_dirty = learned_dirty;
                        if let Some(sel) = apply_active {
                            self.active_floor = sel;
                            // Lock whatever was taught — floor or surface.
                            self.floor_locked = true;
                        }
                    }

                    // 🧲 job result: browser sessions expanded per character.
                    if let Some(job) = self.detect_job.clone() {
                        if let Some(entries) = job.lock().unwrap().take() {
                            self.detect_job = None;
                            self.sync_detected_idx = 0;
                            if entries.is_empty() {
                                self.sync_status = Some(
                                    "✗ no sessions found — log into hoyolab.com first".into(),
                                );
                            } else {
                                let n = entries.len();
                                let first_uid = entries[0].uid;
                                self.sync_cookie = entries[0].cookie.clone();
                                if let Some(uid) = first_uid {
                                    self.sync_profile_uid = uid.to_string();
                                }
                                self.sync_status = Some(format!(
                                    "✓ session 1/{n}: {}",
                                    entries[0].source,
                                ));
                                if n > 1 {
                                    self.sync_open = true;
                                }
                            }
                            self.sync_detected = entries;
                        }
                    }
                    self.label_search = label_search;

                    // Teleport → floor auto-selection. Two strategies:
                    //
                    // 1. SAME-SCENE teleports (surface y range, -100..1000):
                    //    nearest-pin ownership matching — the arrival canvas
                    //    position is reliable, so the nearest pin's floor
                    //    ownership is authoritative.
                    // 2. CROSS-SCENE teleports (instanced y, >1000 or <-200):
                    //    the surface transform produces garbage canvas coords,
                    //    so use the PREVIOUS surface position instead — find
                    //    the floor whose entrance pin was nearest.
                    //
                    // Layer packets (5991) OUTRANK these heuristics: both fire
                    // around the same transition, but the position sample
                    // lags the layer packet by up to a few seconds — without
                    // this guard the pin-match would overwrite the layer
                    // decision right after it lands (and its no-match path
                    // would unlock a fresh surface lock).
                    let layer_recent = self
                        .layer_authoritative_at
                        .map(|t| {
                            t.elapsed()
                                < std::time::Duration::from_secs(10)
                        })
                        .unwrap_or(false);
                    if layer_recent && !self.pending_teleports.is_empty() {
                        tracing::debug!(
                            "teleport floor-match skipped — layer packet \
                             decided within the last 10 s"
                        );
                        self.pending_teleports.clear();
                    }
                    if !self.pending_teleports.is_empty() {
                        if let Some(pd) = pin_data.as_ref() {
                            if !pd.floors.is_empty() {
                                let xf = {
                                    let (sx, sy) = self.cal_scale
                                        .unwrap_or((
                                            md.world_scale,
                                            md.world_scale,
                                        ));
                                    let (ox, oy) = self.calibrate_offset
                                        .unwrap_or((0.0, 0.0));
                                    Xform { sx, sy, ox, oy }
                                };
                                let events = std::mem::take(
                                    &mut self.pending_teleports,
                                );
                                for (tx, ty, tz, tdy) in &events {
                                    let surface_range =
                                        (-100.0..=1_000.0).contains(ty);
                                    if surface_range {
                                        // ── Strategy 1: same-scene pin match ──
                                        let (cx, cy) = xf.apply(
                                            md.origin, *tx, *tz);
                                        let near = pd.index.query(
                                            cx - 120.0, cy - 120.0,
                                            cx + 120.0, cy + 120.0);
                                        // Nearest floor-owned waypoint per
                                        // floor — stacked floors can both
                                        // own pins near the arrival point.
                                        let mut per_floor: Vec<(f64, usize)>
                                            = Vec::new();
                                        for idx in near {
                                            let pin = &pd.pins[idx];
                                            // Waypoint pins only — floor
                                            // ownership is meaningful for
                                            // fast-travel points.
                                            if pin.label_id != 3 {
                                                continue;
                                            }
                                            let d = ((pin.x - cx).powi(2)
                                                + (pin.y - cy).powi(2))
                                                .sqrt();
                                            if d >= 120.0 {
                                                continue;
                                            }
                                            for (fi, f) in pd.floors
                                                .iter().enumerate()
                                            {
                                                if f.point_ids
                                                    .contains(&pin.id)
                                                {
                                                    if let Some(e) =
                                                        per_floor
                                                            .iter_mut()
                                                            .find(|(_, ef)| {
                                                                *ef == fi
                                                            })
                                                    {
                                                        if d < e.0 {
                                                            e.0 = d;
                                                        }
                                                    } else {
                                                        per_floor.push((
                                                            d, fi,
                                                        ));
                                                    }
                                                    break;
                                                }
                                            }
                                        }
                                        per_floor.sort_by(|a, b| {
                                            a.0.partial_cmp(&b.0)
                                                .unwrap_or(
                                                    std::cmp::Ordering::Equal,
                                                )
                                        });
                                        // Stacked-floor disambiguation: a
                                        // big height jump means a different
                                        // floor than the one we left — when
                                        // the nearest match is that same
                                        // floor and another floor also owns
                                        // a nearby pin, prefer the other.
                                        let pick = if *tdy > 250.0 {
                                            let nearest = per_floor
                                                .first()
                                                .map(|(_, f)| *f);
                                            match self.active_floor {
                                                Some(prev)
                                                    if nearest
                                                        == Some(prev)
                                                        && per_floor.len()
                                                            > 1 =>
                                                {
                                                    Some(per_floor[1].1)
                                                }
                                                _ => nearest,
                                            }
                                        } else {
                                            per_floor.first().map(|(_, f)| *f)
                                        };
                                        match pick {
                                            Some(fi) => {
                                                tracing::info!(
                                                    "teleport pin-match → floor {fi} ({}, Δy {tdy:.0})",
                                                    pd.floors[fi].name
                                                );
                                                self.active_floor =
                                                    Some(fi);
                                                self.floor_locked = true;
                                            }
                                            None => {
                                                // No floor-owned waypoint
                                                // near → surface
                                                self.active_floor = None;
                                                self.floor_locked = false;
                                            }
                                        }
                                    } else {
                                        // ── Strategy 2: cross-scene entrance
                                        //    match — use pre-entry surface
                                        //    position to find the floor
                                        //    whose entrance was nearest ──
                                        if let Some((px, _, pz)) = player {
                                            let (dx, dy) = xf.apply(
                                                md.origin, px, pz);
                                            let near = pd.index.query(
                                                dx - 200.0, dy - 200.0,
                                                dx + 200.0, dy + 200.0);
                                            let mut best: Option<
                                                (f64, u32, usize),
                                            > = None; // (dist, pin_id, floor_idx)
                                            for idx in near {
                                                let pin = &pd.pins[idx];
                                                let d = ((pin.x - dx)
                                                    .powi(2)
                                                    + (pin.y - dy).powi(2))
                                                    .sqrt();
                                                if d > 200.0 {
                                                    continue;
                                                }
                                                for (fi, f) in pd.floors
                                                    .iter().enumerate()
                                                {
                                                    if f.point_ids
                                                        .contains(&pin.id)
                                                    {
                                                        if best.as_ref()
                                                            .map(
                                                                |(bd, _, _)| {
                                                                    d < *bd
                                                                })
                                                            .unwrap_or(
                                                                true)
                                                        {
                                                            best = Some((
                                                                d,
                                                                pin.id as u32,
                                                                fi,
                                                            ));
                                                        }
                                                        break;
                                                    }
                                                }
                                            }
                                                        if let Some((
                                                            d, _, fi,
                                                        )) = best {
                                                            tracing::info!(
                                                                "teleport entrance-match → floor {fi} ({}, {d:.0}px)",
                                                                pd.floors[fi].name
                                                            );
                                                            self.active_floor = Some(fi);
                                                            // Lock: protect \
                                                             // from rect override
                                                            self.floor_locked = true;
                                                        }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Floor (layer) selection: manual override from the
                    // sidebar combobox — locks out auto-follow (a manual
                    // Surface pick sticks too; rect containment must not
                    // fight an explicit choice).
                    if let Some(sel) = floor_select {
                        self.active_floor = sel;
                        self.floor_locked = true;
                    }

                    // ── Floor render stack ──
                    // The in-game map shows every layer from the surface
                    // down to the current depth — shallower ones dimmed.
                    // Stacked layer ids differ only in their last two
                    // digits (…0M = depth M), so each shallower member is
                    // prefix·100 + d for d in 1..=depth, resolved through
                    // the learned table. The active floor always renders
                    // undimmed; without a layer id (teleport / manual
                    // selection) the bare active floor is the stack.
                    let mut floor_stack: Vec<(usize, bool)> =
                        Vec::new(); // (floor idx, dimmed)
                    if let Some(pd) = pin_data.as_ref() {
                        if let Some(layer_id) = self.active_layer {
                            let prefix = layer_id / 100;
                            let depth = (layer_id % 100).max(1);
                            for d in 1..=depth {
                                let lid = prefix * 100 + d;
                                let target = self
                                    .learned_layers
                                    .get(&self.selected_map)
                                    .and_then(|m| m.get(&lid))
                                    .copied();
                                if let Some((g, f)) = target {
                                    // Surface entries aren't floors.
                                    if (g, f) == (0, 0) {
                                        continue;
                                    }
                                    if let Some(fi) = pd
                                        .floors
                                        .iter()
                                        .position(|fl| {
                                            fl.group_id == g
                                                && fl.floor_id == f
                                        })
                                    {
                                        if !floor_stack
                                            .iter()
                                            .any(|(i, _)| *i == fi)
                                        {
                                            floor_stack.push((
                                                fi,
                                                d != depth,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        // Inferred siblings: unlearned shallower layer ids
                        // mapped to the previous floors of the active
                        // floor's group (API floors are ordered
                        // shallow→deep within a group, and stack layer
                        // ids share a prefix).
                        if let Some(layer_id) = self.active_layer {
                            if let Some(fi) = self.active_floor {
                                let floor = &pd.floors[fi];
                                let group_floors: Vec<usize> = pd
                                    .floors
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, f)| {
                                        f.group_id == floor.group_id
                                    })
                                    .map(|(i, _)| i)
                                    .collect();
                                if let Some(pos) = group_floors
                                    .iter()
                                    .position(|i| *i == fi)
                                {
                                    let prefix = layer_id / 100;
                                    let depth =
                                        (layer_id % 100).max(1) as usize;
                                    for d in 1..depth {
                                        let sid = prefix * 100 + d as u64;
                                        let already = self
                                            .learned_layers
                                            .get(&self.selected_map)
                                            .map(|m| {
                                                m.contains_key(&sid)
                                            })
                                            .unwrap_or(false);
                                        if already {
                                            continue;
                                        }
                                        let off = depth - d;
                                        if pos >= off {
                                            if let Some(&tfi) =
                                                group_floors.get(pos - off)
                                            {
                                                if !floor_stack.iter().any(
                                                    |(i, _)| *i == tfi,
                                                ) {
                                                    floor_stack.push((
                                                        tfi, true,
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // The active floor is never dimmed; if it isn't in
                        // the learned stack yet (partial learning), append
                        // it — the deepest layer draws last (on top).
                        if let Some(fi) = self.active_floor {
                            if let Some(entry) = floor_stack
                                .iter_mut()
                                .find(|(i, _)| *i == fi)
                            {
                                entry.1 = false;
                            } else {
                                floor_stack.push((fi, false));
                            }
                        }
                    }
                    let mut floor_stack_render: Vec<(
                        &pins::FloorInfo,
                        Option<egui::TextureId>,
                        bool,
                    )> = Vec::new();
                    if let Some(pd) = pin_data.as_ref() {
                        for (fi, dimmed) in &floor_stack {
                            if let Some(floor) = pd.floors.get(*fi) {
                                let tex_id = Self::ensure_floor_overlay(
                                    &mut self.floor_overlays,
                                    floor,
                                    ui.ctx(),
                                );
                                floor_stack_render
                                    .push((floor, tex_id, *dimmed));
                            }
                        }
                    }
                    let mut quick_toggle: Option<(u64, u32)> = None;
                    // Bulk mark/unmark from the sidebar context menu: apply
                    // locally, then mirror to HoYoLab via the batch endpoint.
                    if let Some((label_id, mark)) = bulk_action {
                        if let Some(pd) = pin_data.as_ref() {
                            let pin_ids: Vec<u64> = pd.pins.iter()
                                .filter(|p| p.label_id == label_id)
                                .map(|p| p.id).collect();
                            let bucket = self.active_uid.unwrap_or(0);
                            let set = self.completed
                                .entry((bucket, self.selected_map)).or_default();
                            let mut n = 0;
                            if mark {
                                for id in &pin_ids { if set.insert(*id) { n += 1; } }
                            } else {
                                for id in &pin_ids { if set.remove(id) { n += 1; } }
                            }
                            Self::write_completed(&self.completed);
                            let label_name = pd.labels.iter()
                                .find(|l| l.id == label_id)
                                .map(|l| l.name.clone())
                                .unwrap_or_else(|| "Label".into());
                            let verb = if mark { "collected" } else { "unmarked" };
                            tracing::info!("bulk {verb} {n} x {label_name}");
                            self.auto_notes.push((
                                format!("✓ {n} {label_name} {verb}"),
                                std::time::Instant::now(),
                            ));
                            // Mirror remotely (batch, is_delete = !mark).
                            if self.sync_auto && !self.sync_cookie.trim().is_empty() {
                                let (map_id, cookie) =
                                    (self.selected_map, self.sync_cookie.clone());
                                let items: Vec<(u64, bool)> = pin_ids
                                    .iter().map(|&id| (id, !mark)).collect();
                                std::thread::spawn(move || {
                                    match sync::batch_mark(map_id, &items, &cookie) {
                                        Ok((ok, failed)) => tracing::info!(
                                            "HoYoLab bulk push: {ok} ok, {failed} failed"),
                                        Err(e) => tracing::warn!(
                                            "HoYoLab bulk push failed: {e:#}"),
                                    }
                                });
                            }
                        }
                    }

                    // Right-click quick toggle: instantly mark/unmark the pin.
                    if let Some((pin_id, label_id)) = quick_toggle {
                        let bucket = self.active_uid.unwrap_or(0);
                        let set = self.completed
                            .entry((bucket, self.selected_map)).or_default();
                        let now_done = if set.contains(&pin_id) {
                            set.remove(&pin_id);
                            false
                        } else {
                            set.insert(pin_id);
                            true
                        };
                        Self::write_completed(&self.completed);
                        let name = pin_data.as_ref()
                            .and_then(|pd| pd.labels.iter().find(|l| l.id == label_id))
                            .map(|l| l.name.clone())
                            .unwrap_or_else(|| "Pin".into());
                        let icon = if now_done { "✓" } else { "↩" };
                        let verb = if now_done { "collected" } else { "unmarked" };
                        tracing::info!("{icon} {name} {verb} (quick toggle)");
                        self.auto_notes.push((
                            format!("{icon} {name} {verb}"),
                            std::time::Instant::now(),
                        ));
                        // Mirror to HoYoLab when auto-sync is on.
                        if self.sync_auto && !self.sync_cookie.trim().is_empty() {
                            let (map_id, cookie) =
                                (self.selected_map, self.sync_cookie.clone());
                            std::thread::spawn(move || {
                                let res = if now_done {
                                    sync::add_mark(map_id, pin_id, &cookie)
                                } else {
                                    sync::delete_mark(map_id, pin_id, &cookie)
                                };
                                if let Err(e) = res {
                                    tracing::warn!("HoYoLab mirror failed: {e:#}");
                                }
                            });
                        }
                    }

                    // NOTE: there is NO geometric sub-map auto-switching.
                    // Every sub-map (7/9/34/36/37/40) is a separate
                    // REGION with its own local coordinate system —
                    // cross-map canvas containment is meaningless
                    // (verified: the Chasm mines report small local
                    // coords like (402, 419, 396), just like the moon).
                    // Map authority is: scene ids + region broadcasts
                    // (worker) → manual selection (locked) in between.

                    if start_sync && self.sync_job.is_none() {
                        // Inline sync_uid(): &self method conflicts with the
                        // live &mut self.state borrow in this arm.
                        let uid = self.sync_profile_idx
                            .and_then(|i| self.sync_profiles.get(i))
                            .and_then(|p| p.uid)
                            .or_else(|| self.sync_detected
                                .get(self.sync_detected_idx)
                                .and_then(|d| d.uid))
                            .or(self.active_uid);
                        if let Some(uid) = uid {
                            let map_id = self.selected_map;
                            let cookie = self.sync_cookie.clone();
                            let job: Arc<Mutex<Option<anyhow::Result<sync::MarksResult>>>> =
                                Arc::new(Mutex::new(None));
                            let j2 = job.clone();
                            std::thread::spawn(move || {
                                *j2.lock().unwrap() = Some(sync::fetch_marks(map_id, uid, &cookie));
                            });
                            self.sync_job = Some(job);
                            self.sync_status = Some("Fetching marks…".into());
                        } else {
                            self.sync_status = Some(
                                "✗ no UID — set one on the profile or log into the game first".into(),
                            );
                        }
                    }

                    // Export: push locally-collected pins to HoYoLab.
                    if start_export && self.export_job.is_none() {
                        let uid = self.sync_profile_idx
                            .and_then(|i| self.sync_profiles.get(i))
                            .and_then(|p| p.uid)
                            .or_else(|| self.sync_detected
                                .get(self.sync_detected_idx)
                                .and_then(|d| d.uid))
                            .or(self.active_uid);
                        if let Some(uid) = uid {
                            if self.sync_cookie.trim().is_empty() {
                                self.sync_status =
                                    Some("✗ No cookie — 🧲 detect, paste, or pick a profile".into());
                            } else {
                                let map_id = self.selected_map;
                                // Bucket follows the sync uid (profile or active game).
                                let local: Vec<u64> = self.completed
                                    .get(&(uid, map_id))
                                    .map(|s| s.iter().copied().collect())
                                    .unwrap_or_default();
                                let cookie = self.sync_cookie.clone();
                                let progress = Arc::new(Mutex::new(String::from(
                                    "⬆ Preparing…",
                                )));
                                let result: Arc<
                                    Mutex<Option<anyhow::Result<(usize, usize)>>>,
                                > = Arc::new(Mutex::new(None));
                                let (p2, r2) = (progress.clone(), result.clone());
                                std::thread::spawn(move || {
                                    let res = sync::push_missing(
                                        map_id, uid, local, &cookie,
                                        &|done, total| {
                                            *p2.lock().unwrap() =
                                                format!("⬆ Pushing {done}/{total}…");
                                        },
                                    );
                                    *r2.lock().unwrap() = Some(res);
                                });
                                self.export_job = Some((progress, result));
                            }
                        } else {
                            self.sync_status = Some(
                                "✗ no UID — set one on the profile or log into the game first".into(),
                            );
                        }
                    }
                    if let Some((prog, result)) = self.export_job.clone() {
                        if let Some(res) = result.lock().unwrap().take() {
                            self.export_job = None;
                            match res {
                                Ok((pushed, failed)) => {
                                    self.sync_status = Some(if pushed == 0 {
                                        "⬆ Already up to date (nothing to push)".into()
                                    } else {
                                        format!("⬆ Pushed {pushed} pins to HoYoLab ({failed} failed)")
                                    });
                                    self.auto_notes.push((
                                        format!("⬆ Pushed {pushed} pins to HoYoLab"),
                                        std::time::Instant::now(),
                                    ));
                                }
                                Err(e) => {
                                    self.sync_status = Some(format!("✗ {e:#}"));
                                }
                            }
                        }
                    }

                    // ── Canvas ──
                    let auto_notes = self.auto_notes.clone();
                    let mut center_request = self.center_request;
                    let mut auto_map = self.auto_map;
                    // Assign the live position to a calibration frame:
                    // same-scene generation first, then any frame with a
                    // close point (re-entry). Keeps the current frame when
                    // nothing claims the position.
                    if let Some((x, _, z)) = player {
                        if !self.cal_points.is_empty() {
                            if let Some(idx) = assign_frame(
                                &self.cal_points, x, z, self.scene_gen,
                            ) {
                                self.active_cal_frame = idx;
                            }
                        }
                    }
                    let frame_solutions = solve_all_frames(
                        &self.cal_points, md.origin, md.world_scale,
                    );
                    let active_idx = self
                        .active_cal_frame
                        .min(frame_solutions.len().saturating_sub(1));
                    let (xf, frame_res) = frame_solutions
                        .get(active_idx)
                        .copied()
                        .unwrap_or_else(|| {
                            // No frames yet — legacy single offset/scale.
                            let (sx, sy) = self.cal_scale
                                .unwrap_or((md.world_scale, md.world_scale));
                            let (ox, oy) = self
                                .calibrate_offset
                                .unwrap_or((0.0, 0.0));
                            (Xform { sx, sy, ox, oy }, 0.0)
                        });
                    let total_pts: usize =
                        self.cal_points.iter().map(|f| f.len()).sum();
                    // "Far from every calibration point" = an uncalibrated
                    // frame (a new layer) — tell the user what to do.
                    let mut cal_unassigned = false;
                    if let Some((x, _, z)) = player {
                        if !self.cal_points.is_empty()
                            && assign_frame(
                                &self.cal_points, x, z, self.scene_gen,
                            )
                            .is_none()
                        {
                            cal_unassigned = true;
                        }
                    }
                    let cal_info = (total_pts, frame_res, cal_unassigned);

                    // Resolve a pending map-layer report (5991): the
                    // learned mapping switches instantly; an unknown id is
                    // learned from geometry once the position enters a
                    // floor overlay (smallest containing rect wins).
                    if let Some(layer_id) = self.pending_layer {
                        if let Some(pd) = pin_data.as_ref() {
                            if !pd.floors.is_empty() {
                                let known = self
                                    .learned_layers
                                    .get(&self.selected_map)
                                    .and_then(|m| m.get(&layer_id))
                                    .copied();
                                // Sentinel (0, 0) = surface (learned
                                // "this layer is outdoors").
                                let mut surface_pick = false;
                                let resolved: Option<usize> =
                                    match known {
                                        Some((0, 0)) => {
                                            surface_pick = true;
                                            None
                                        }
                                        Some((gid, fid)) => {
                                            pd.floors.iter().position(
                                                |f| {
                                                    f.group_id == gid
                                                        && f.floor_id == fid
                                                },
                                            )
                                        }
                                        None => {
                                            if let Some((x, _, z)) = player {
                                                let (dx, dy) =
                                                    xf.apply(md.origin, x, z);
                                                let (rx, ry) = (
                                                    dx - md.origin.0,
                                                    dy - md.origin.1,
                                                );
                                                let mut best: Option<(
                                                    f64,
                                                    usize,
                                                )> = None;
                                                for (i, f) in pd
                                                    .floors
                                                    .iter()
                                                    .enumerate()
                                                {
                                                    let m = 10.0;
                                                    if rx > f.rect.0 + m
                                                        && rx < f.rect.2 - m
                                                        && ry > f.rect.1 + m
                                                        && ry < f.rect.3 - m
                                                    {
                                                        let area = (f.rect.2
                                                            - f.rect.0)
                                                            * (f.rect.3
                                                                - f.rect.1);
                                                        if best.is_none()
                                                            || area
                                                                < best
                                                                    .unwrap()
                                                                    .0
                                                        {
                                                            best = Some((
                                                                area, i,
                                                            ));
                                                        }
                                                    }
                                                }
                                                if let Some((_, i)) = best {
                                                    // Learn: layer id →
                                                    // (group, floor).
                                                    self.learned_layers
                                                        .entry(
                                                            self
                                                                .selected_map,
                                                        )
                                                        .or_default()
                                                        .insert(
                                                            layer_id,
                                                            (
                                                                pd.floors[i]
                                                                    .group_id,
                                                                pd.floors[i]
                                                                    .floor_id,
                                                            ),
                                                        );
                                                    self.learned_layers_dirty =
                                                        true;
                                                }
                                                best.map(|(_, i)| i)
                                            } else {
                                                None
                                            }
                                        }
                                    };
                                if surface_pick {
                                    tracing::info!(
                                        "map layer {layer_id} → surface"
                                    );
                                    self.active_floor = None;
                                    // Lock surface — same reasoning as the
                                    // default-layer report: the packet
                                    // outranks rect containment.
                                    self.floor_locked = true;
                                    self.pending_layer = None;
                                    self.layer_authoritative_at =
                                        Some(std::time::Instant::now());
                                } else if let Some(fi) = resolved {
                                    tracing::info!(
                                        "map layer {layer_id} → floor {fi} ({})",
                                        pd.floors[fi].name
                                    );
                                    self.active_floor = Some(fi);
                                    self.floor_locked = true;
                                    self.pending_layer = None;
                                    self.active_layer = Some(layer_id);
                                    self.layer_authoritative_at =
                                        Some(std::time::Instant::now());
                                }
                            } else {
                                // Flat map — nothing to resolve.
                                self.pending_layer = None;
                            }
                        }
                    }

                    // Floor auto-follow: selects the floor whose overlay
                    // contains the dot; returns to surface once clear.
                    // Skipped when the position is unclaimed or when the
                    // user manually locked a floor (their pick wins until
                    // they return to the surface).
                    if self.auto_map && !cal_unassigned && !self.floor_locked {
                        if let Some(pd) = pin_data.as_ref() {
                            if !pd.floors.is_empty() {
                                if let Some((x, _, z)) = player {
                                    let (dx, dy) = xf.apply(md.origin, x, z);
                                    let (rx, ry) =
                                        (dx - md.origin.0, dy - md.origin.1);
                                    // Inside any overlay rect (with margin)?
                                    let mut best_floor: Option<(f64, usize)> = None;
                                    for (i, f) in pd.floors.iter().enumerate() {
                                        let m = 10.0;
                                        if rx > f.rect.0 + m
                                            && rx < f.rect.2 - m
                                            && ry > f.rect.1 + m
                                            && ry < f.rect.3 - m
                                        {
                                            let area = (f.rect.2 - f.rect.0)
                                                * (f.rect.3 - f.rect.1);
                                            if best_floor.is_none()
                                                || area < best_floor
                                                    .map(|(a, _)| a)
                                                    .unwrap_or(f64::INFINITY)
                                            {
                                                best_floor = Some((area, i));
                                            }
                                        }
                                    }
                                    match best_floor {
                                        Some((_, i)) => {
                                            if self.active_floor != Some(i)
                                            {
                                                self.active_floor = Some(i);
                                            }
                                        }
                                        None => {
                                            // Outside all overlays → surface
                                            self.active_floor = None;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let mut cal_action: Option<CalAction> = None;
                    egui::CentralPanel::default().show_inside(ui, |ui| {
                        Self::canvas(ui, tex, store, &md, player,
                            &mut pan, &mut zoom, &mut follow, &mut calibrating,
                            &mut view_init, xf, cal_info, &mut cal_action,
                            pin_data.as_deref(), &pin_filter, &icon_textures,
                            &floor_stack_render,
                            &completed, &mut selected, &auto_notes,
                            &mut quick_toggle, &mut center_request, &mut auto_map);
                    });
                    self.center_request = center_request;
                    // Cycling 🗺 auto (off→on) releases a manual map lock.
                    if self.auto_map && !auto_map {
                        self.manual_map_lock = false;
                    }
                    self.auto_map = auto_map;

                    // Calibration actions: add a point (auto-solve, frame
                    // auto-assigned by proximity; new frames for new layers)
                    // or clear everything.
                    if let Some(action) = cal_action {
                        match action {
                            CalAction::AddPoint { mx, my } => {
                                if let Some((wx, _, wz)) = player {
                                    // Frame assignment: same scene generation
                                    // first (layers are separate scenes), then
                                    // any frame with a close point; otherwise
                                    // a new frame.
                                    let frame_idx = assign_frame(
                                        &self.cal_points, wx, wz, self.scene_gen,
                                    )
                                    .unwrap_or_else(|| {
                                        self.cal_points.push(Vec::new());
                                        self.cal_points.len() - 1
                                    });
                                    self.cal_points[frame_idx].push(CalPoint {
                                        wx, wz, mx, my, scene_gen: self.scene_gen,
                                    });
                                    self.active_cal_frame = frame_idx;
                                    let solutions = solve_all_frames(
                                        &self.cal_points,
                                        md.origin,
                                        md.world_scale,
                                    );
                                    let (xf2, res) = solutions[frame_idx];
                                    self.cal_scale = Some((xf2.sx, xf2.sy));
                                    self.calibrate_offset =
                                        Some((xf2.ox, xf2.oy));
                                    Self::save_calibration(
                                        md.map_id, self.calibrate_offset,
                                        self.cal_scale, &self.cal_points,
                                    );
                                    let n = self.cal_points[frame_idx].len();
                                    let frames = self.cal_points.len();
                                    self.auto_notes.push((
                                        format!(
                                            "📍 cal L{frame_idx} {n} pts — {:.3}×/{:.3}×, err {res:.1}px ({frames} layer{})",
                                            xf2.sx, xf2.sy,
                                            if frames == 1 { "" } else { "s" },
                                        ),
                                        std::time::Instant::now(),
                                    ));
                                    tracing::info!(
                                        "calibration L{frame_idx}: {n} pts scale ({}, {}) offset ({}, {}) err {res}",
                                        xf2.sx, xf2.sy, xf2.ox, xf2.oy
                                    );
                                }
                            }
                            CalAction::Clear => {
                                self.cal_points.clear();
                                self.active_cal_frame = 0;
                                self.cal_scale = None;
                                self.calibrate_offset = None;
                                Self::save_calibration(md.map_id, None, None, &[]);
                                self.auto_notes.push((
                                    "📍 calibration cleared".into(),
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }

                    // Stream tiles at a lively repaint rate while busy.
                    if store.has_pending() {
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(80));
                    }
                }
            }
        }

        // ── Pin details popup ──
        let icon_textures = self.icon_textures.clone();
        if let Some(sel) = selected.clone() {
            let mut keep = true;
            let done = completed.contains(&sel.pin_id);
            egui::Window::new("📍 Pin")
                .id(egui::Id::new("pin_popup"))
                .open(&mut keep)
                .collapsible(false)
                .resizable(false)
                .min_width(240.0)
                .show(ui.ctx(), |ui| {
                    if let Some(pd) = pin_data.as_ref() {
                        let label = pd.labels.iter().find(|l| l.id == sel.label_id);
                        ui.horizontal(|ui| {
                            if let (Some(label), Some(tex)) =
                                (label, icon_textures.get(&sel.label_id))
                            {
                                ui.image((tex.id(), egui::vec2(28.0, 28.0)));
                                ui.heading(&label.name);
                            } else {
                                ui.heading(format!("Pin #{}", sel.pin_id));
                            }
                        });
                        if let Some(label) = label {
                            // Show the label's HoYoLab group name.
                            let gname = pd.groups.iter()
                                .find(|g| g.label_indices.iter()
                                    .any(|&li| pd.labels[li].id == label.id))
                                .map(|g| g.name.clone());
                            if let Some(g) = gname { ui.weak(g); }
                        }
                        if let Some(pin) = pd.pins.iter().find(|p| p.id == sel.pin_id) {
                            ui.monospace(format!("map px: ({:.0}, {:.0})",
                                pin.x - pd.map_info.origin.0, pin.y - pd.map_info.origin.1));
                            if ui.button("🎯 Center map here").clicked() {
                                self.center_request = Some((pin.x, pin.y));
                            }
                        }
                    } else {
                        ui.heading(format!("Pin #{}", sel.pin_id));
                    }
                    ui.separator();
                    let txt = if done { "↩ Undo collected" } else { "✓ Mark collected" };
                    if ui.button(txt).clicked() {
                        let set = self.completed.entry((self.bucket(), sel.map_id)).or_default();
                        if done { set.remove(&sel.pin_id); } else { set.insert(sel.pin_id); }
                        self.save_completed();
                        // Mirror the action to HoYoLab when auto-sync is on.
                        if self.sync_auto && !self.sync_cookie.trim().is_empty() {
                            let (map_id, pid, cookie) = (
                                sel.map_id, sel.pin_id, self.sync_cookie.clone(),
                            );
                            let op = if done { "unmark" } else { "mark" };
                            std::thread::spawn(move || {
                                let res = if done {
                                    sync::delete_mark(map_id, pid, &cookie)
                                } else {
                                    sync::add_mark(map_id, pid, &cookie)
                                };
                                match res {
                                    Ok(()) => tracing::info!("HoYoLab {op} {pid} ok"),
                                    Err(e) => tracing::warn!("HoYoLab {op} {pid} failed: {e:#}"),
                                }
                            });
                        }
                    }
                });
            if !keep { selected = None; }
        }
        self.selected = selected;

        self.pan=pan; self.zoom=zoom; self.follow=follow; self.calibrating=calibrating;
        // Persist learned layer-id → floor mappings (deferred from the
        // state match, which holds a mutable borrow).
        if self.learned_layers_dirty {
            let snapshot = self.learned_layers.clone();
            Self::write_learned_layers(&snapshot);
            self.learned_layers_dirty = false;
        }
        self.view_init=view_init;
        if filter_changed { self.pin_filter=pin_filter; self.pin_filter.save(&Self::data_dir(), self.bucket(), selected_map); }

        // Region-broadcast switch (authoritative) — merged with any manual
        // selection from this frame. Region packets outrank the manual
        // lock: crossing into the Chasm / the moon / back to the surface
        // re-takes control and re-arms geometric switching.
        if let Some(r) = self.pending_region.take() {
            self.manual_map_lock = false;
            if new_map.is_none() {
                new_map = Some(r);
            }
        }

        // Map switch: reset everything and auto-start the new map.
        if let Some(id) = new_map {
            if id != self.selected_map {
                tracing::info!(from = self.selected_map, to = id, "switching map");
                let old = self.selected_map;
                self.pin_filter.save(&Self::data_dir(), self.bucket(), old);
                self.selected_map = id;
                self.pin_filter = pins::FilterState::load(&Self::data_dir(), self.bucket(), id);
                self.state = MapLoadState::Idle;   // drop old texture+data
                self.pin_state = PinState::NotLoaded;
                self.icons_initialized = false;
                self.icon_textures.clear();
                self.tiles = None;                 // drop texture LRU
                self.selected = None;              // close popup
                self.active_floor = None;
                self.pending_layer = None;         // layer ids are map-scoped
                self.active_layer = None;
                self.floor_locked = false;         // surface locks don't carry over
                self.floor_overlays.clear();
                let (off, scale, pts) = Self::load_calibration(id);
                self.calibrate_offset = off;
                self.cal_scale = scale;
                self.cal_points = pts;
                self.pan = egui::Vec2::ZERO; self.view_init = false; self.follow = true;
                self.start_pin_load();
                self.start_map_download(cache_dir);
            }
        }
        if let Some(n) = next_state { self.state = n; }
    }

    /// Chest-open matching with the challenge dwell rule:
    ///  1. Dwell: an uncollected challenge pin the player stood at (≥3
    ///     samples within 40px, 8s–5min ago) with the chest event within
    ///     250px → mark the challenge (spawned reward chest).
    ///  2. Otherwise: nearest un-collected chest pin within 60px (type-word
    ///     preference).
    /// Returns the toast text.
    fn match_chest_or_challenge(
        md: &MapData, pd: &PinData,
        completed: &mut HashMap<(u32, u32), std::collections::HashSet<u64>>,
        bucket: u32, xf: Xform,
        ev: &ChestMark,
        pos_history: &[(f32, f32, std::time::Instant)],
    ) -> Option<String> {
        let (cx, cy) = xf.apply(md.origin, ev.x, ev.z);
        let done_set = completed.get(&(bucket, md.map_id));

        let challenge_labels = pd.semantic_labels(PinCategory::Challenges);

        // ── Rule 1: challenge dwell anchor ──
        let cands = pd.index.query(cx - 250.0, cy - 250.0, cx + 250.0, cy + 250.0);
        for idx in cands {
            let pin = &pd.pins[idx];
            if !challenge_labels.contains(&pin.label_id) { continue; }
            if done_set.map(|s| s.contains(&pin.id)).unwrap_or(false) { continue; }
            let d_event = ((pin.x - cx).powi(2) + (pin.y - cy).powi(2)).sqrt();
            if d_event > 250.0 { continue; }
            let dwell = pos_history.iter().filter(|(hx, hz, t)| {
                let age = t.elapsed().as_secs_f32();
                if !(8.0..300.0).contains(&age) { return false; }
                let (px, py) = xf.apply(md.origin, *hx, *hz);
                let d = ((pin.x - px).powi(2) + (pin.y - py).powi(2)).sqrt();
                d <= 40.0
            }).count();
            if dwell >= 3 {
                completed.entry((bucket, md.map_id)).or_default().insert(pin.id);
                let name = pd.labels.iter().find(|l| l.id == pin.label_id)
                    .map(|l| l.name.clone()).unwrap_or_else(|| "Challenge".into());
                tracing::info!("challenge completed: {name} (dwell {dwell}, event {d_event:.0}px)");
                return Some(format!("🏁 {name} completed"));
            }
        }

        // ── Rule 2: regular chest pin ──
        let chest_labels = pd.semantic_labels(PinCategory::Chests);
        let name = Self::match_pin(
            md, pd, completed, bucket, xf,
            ev.x, ev.z, 60.0, &chest_labels, Some(ev.kind.as_str()),
        )?;
        tracing::info!("auto-collected: {name}");
        Some(format!("✓ {name} auto-collected"))
    }

    /// Match a detected event (chest open / oculus collect) to the nearest
    /// un-collected API pin among `label_ids` within `radius` map px,
    /// preferring pins whose type word matches `hint`. Marks it collected
    /// and returns the pin's label name.
    fn match_pin(
        md: &MapData, pd: &PinData,
        completed: &mut HashMap<(u32, u32), std::collections::HashSet<u64>>,
        bucket: u32, xf: Xform,
        x: f32, z: f32, radius: f64,
        label_ids: &std::collections::HashSet<u32>,
        hint: Option<&str>,
    ) -> Option<String> {
        let r = radius;
        let (cx, cy) = xf.apply(md.origin, x, z);
        let cands = pd.index.query(cx - r, cy - r, cx + r, cy + r);

        let done_set = completed.get(&(bucket, md.map_id));

        let mut best: Option<(f64, u64, u32)> = None; // (effective dist, pin id, label)
        for idx in cands {
            let pin = &pd.pins[idx];
            if !label_ids.contains(&pin.label_id) { continue; }
            if done_set.map(|s| s.contains(&pin.id)).unwrap_or(false) { continue; }
            let d = ((pin.x - cx).powi(2) + (pin.y - cy).powi(2)).sqrt();
            if d > r { continue; }
            let name = pd.labels.iter().find(|l| l.id == pin.label_id)
                .map(|l| l.name.to_lowercase()).unwrap_or_default();
            let eff = match hint {
                Some(h) if name.contains(h) => d - 25.0,
                _ => d,
            };
            if best.as_ref().map(|(bd, ..)| eff < *bd).unwrap_or(true) {
                best = Some((eff, pin.id, pin.label_id));
            }
        }
        let (_, pin_id, label_id) = best?;
        completed.entry((bucket, md.map_id)).or_default().insert(pin_id);
        let name = pd.labels.iter().find(|l| l.id == label_id)
            .map(|l| l.name.clone()).unwrap_or_else(|| "Pin".into());
        Some(name)
    }

    /// Combo box for map selection; returns Some(map_id) when changed.
    fn map_selector(ui: &mut egui::Ui, id: &str, selected: u32) -> Option<u32> {
        let maps = pins::available_maps();
        let cur = maps.iter().position(|(i,_)| *i == selected).unwrap_or(0);
        let mut sel = cur;
        egui::ComboBox::from_id_salt(id)
            .selected_text(maps[cur].1)
            .width(230.0)
            .show_ui(ui, |ui| {
                for (i,(_,name)) in maps.iter().enumerate() {
                    ui.selectable_value(&mut sel, i, *name);
                }
            });
        (sel != cur).then_some(maps[sel].0)
    }

    // ── Sidebar: individual label toggles grouped by HoYoLab's own groups ──
    #[allow(clippy::too_many_arguments)]
    fn sidebar(ui: &mut egui::Ui, pd: &PinData, filter: &mut pins::FilterState,
               icons: &HashMap<u32, egui::TextureHandle>, changed: &mut bool,
               completed: &std::collections::HashSet<u64>,
               bulk: &mut Option<(u32, bool)>,
               search: &mut String) {
        ui.heading("📋 Pin Filters");
        if ui.checkbox(&mut filter.hide_completed, "Hide collected").changed() { *changed = true; }
        if ui.checkbox(&mut filter.flatten_layers, "Show all layers' pins")
            .on_hover_text(
                "Ignore layer partitioning: every pin renders fully \
                 opaque on every view. Off = surface view shows only \
                 surface pins, floor views only their active floor.")
            .changed()
        { *changed = true; }
        ui.horizontal(|ui| {
            // Fixed width: available_width() in an auto-sized side panel is
            // the whole screen, which would stretch the panel.
            ui.add(egui::TextEdit::singleline(search)
                .desired_width(142.0)
                .hint_text("🔍 search labels…"));
            if !search.is_empty() && ui.button("⨯").clicked() {
                search.clear();
            }
        });
        ui.separator();

        let query = search.trim().to_lowercase();
        let searching = !query.is_empty();

        // Completed counts per label.
        let mut done_by_label: HashMap<u32, usize> = HashMap::new();
        for pid in completed {
            if let Some(lid) = pd.pin_label.get(pid) {
                *done_by_label.entry(*lid).or_default() += 1;
            }
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for group in &pd.groups {
                let total: usize = group.label_indices.iter()
                    .map(|&i| pd.labels[i].pin_count).sum();
                if total == 0 { continue; }

                // While searching, only show groups (and rows) that match.
                let visible_indices: Vec<usize> = if searching {
                    group.label_indices.iter()
                        .filter(|&&i| pd.labels[i].name.to_lowercase().contains(&query))
                        .copied().collect()
                } else {
                    group.label_indices.clone()
                };
                if visible_indices.is_empty() { continue; }

                let ids: Vec<u32> = visible_indices.iter()
                    .map(|&i| pd.labels[i].id).collect();
                let all_on = filter.all_enabled(&ids);
                let any_on = filter.any_enabled(&ids);
                let done_total: usize = visible_indices.iter()
                    .map(|&i| done_by_label.get(&pd.labels[i].id).copied().unwrap_or(0)).sum();

                egui::CollapsingHeader::new(
                    if done_total > 0 {
                        format!("{} ({}/{})", group.name, done_total, total)
                    } else {
                        format!("{} ({})", group.name, total)
                    }
                )
                .default_open(all_on)
                // While searching, force groups open every frame (default_open
                // only applies before the header's state is remembered).
                .open(if searching { Some(true) } else { None })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let mut all = all_on;
                        if ui.checkbox(&mut all,
                            if all_on {"✓ All"} else if any_on {"▬ Partial"} else {"✗ None"})
                            .changed() {
                            filter.set_ids(&ids, all);
                            *changed = true;
                        }
                    });

                    for &li in &visible_indices {
                        let label = &pd.labels[li];
                        let done = done_by_label.get(&label.id).copied().unwrap_or(0);
                        let mut on = filter.is_enabled(label.id);
                        ui.horizontal(|ui| {
                            if let Some(tex) = icons.get(&label.id) {
                                let img = egui::Image::new((tex.id(), egui::vec2(16.0, 16.0)));
                                if done > 0 {
                                    ui.add(img.tint(egui::Color32::from_rgba_unmultiplied(
                                        255,255,255,120)));
                                } else {
                                    ui.add(img);
                                }
                            }
                            let count = if done > 0 {
                                format!("{} ({}/{})", label.name, done, label.pin_count)
                            } else {
                                format!("{} ({})", label.name, label.pin_count)
                            };
                            let cb = ui.checkbox(&mut on, count);
                            if cb.changed() {
                                filter.toggle(label.id);
                                *changed = true;
                            }
                            // Right-click: bulk mark/unmark all pins of this label.
                            cb.context_menu(|ui| {
                                if ui.button(format!("✓ Mark all {} as collected",
                                    label.pin_count)).clicked()
                                {
                                    *bulk = Some((label.id, true));
                                    ui.close();
                                }
                                if ui.button(format!("↩ Unmark all ({} collected)", done))
                                    .clicked()
                                {
                                    *bulk = Some((label.id, false));
                                    ui.close();
                                }
                            });
                        });
                    }
                });
            }
        });
        // (summary footer lives in the fixed bottom section)
    }

    // ── Canvas ──
    #[allow(clippy::too_many_arguments)]
    fn canvas(ui: &mut egui::Ui, overview: &egui::TextureHandle,
              store: &mut TileStore, data: &MapData,
              player: Option<(f32,f32,f32)>,
              pan: &mut egui::Vec2, zoom: &mut f32,
              follow: &mut bool, calibrating: &mut bool, view_init: &mut bool,
              xf: Xform, cal_info: (usize, f64, bool),
              cal_action: &mut Option<CalAction>,
              pin_data: Option<&PinData>, filter: &pins::FilterState,
              icons: &HashMap<u32, egui::TextureHandle>,
              active_floor: &[(&pins::FloorInfo, Option<egui::TextureId>, bool)],
              completed: &std::collections::HashSet<u64>,
              selected: &mut Option<SelectedPin>,
              auto_notes: &[(String, std::time::Instant)],
              quick_toggle: &mut Option<(u64, u32)>,
              center_request: &mut Option<(f64, f64)>,
              auto_map: &mut bool) {
        ui.horizontal(|ui| {
            ui.checkbox(follow, "📍 follow")
                .on_hover_text(
                    "Camera: keep the map view centered on your live position. \
                     Panning or scrolling turns it off; it also can't do \
                     anything until a position is received (login first).");
            ui.checkbox(auto_map, "🗺 auto")
                .on_hover_text(
                    "Auto-switch the displayed sub-map and floor: region \
                     packets (Chasm / Nod-Krai), teleports (waypoint floor \
                     matching) and position (map bounds).");
            if ui.button("🎯 center").clicked() { *follow = true; *zoom = 1.0; }
            if *follow && player.is_none() {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 190, 90),
                    "📍 waiting for position…",
                );
            }
            ui.separator();
            if ui.button(if *calibrating {"◉ click spot…"} else {"📍+ cal"})
                .on_hover_text(
                    "Teleport somewhere, let the dot settle, then click where you \
                     really are. Two or more points solve scale + offset.")
                .clicked()
            {
                *calibrating = !*calibrating;
            }
            if ui.button("🗑cal").clicked() {
                *cal_action = Some(CalAction::Clear);
            }
            ui.separator();
            let cal_txt = if cal_info.0 > 0 {
                format!("{}  {:.3}×/{:.3}×  err {:.1}px  {}pts",
                    data.name, xf.sx, xf.sy, cal_info.1, cal_info.0)
            } else {
                format!("{}  {:.2}×", data.name, zoom)
            };
            ui.weak(cal_txt);
            if cal_info.2 {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 190, 90),
                    "⚠ uncalibrated layer — 📍+ cal anchors it (pick the floor in the sidebar, then click your spot)",
                );
            }
        });

        let size = ui.available_size();
        let (resp, painter) = ui.allocate_painter(size, egui::Sense::click_and_drag());
        let rect = resp.rect;

        // Center-on-pin request from the popup (canvas-relative center).
        if let Some((mx, my)) = center_request.take() {
            *follow = false;
            *pan = (rect.center() - rect.left_top())
                - egui::vec2(mx as f32 * *zoom, my as f32 * *zoom);
        }

        // Initial view: fit map width into the canvas (canvas-relative
        // center — pan is relative to rect.left_top()).
        if !*view_init {
            *zoom = (rect.width() * 0.92 / data.total_size.0 as f32)
                .clamp(Self::min_zoom(data), 6.0);
            *pan = (rect.center() - rect.left_top())
                - egui::vec2(
                    data.total_size.0 as f32,
                    data.total_size.1 as f32) * *zoom / 2.0;
            *view_init = true;
        }

        // Zoom (anchored at cursor). `zoom` = screen px per map px.
        let scroll = resp.ctx.input(|i| i.raw_scroll_delta.y);
        if scroll != 0.0 && resp.hovered() {
            if let Some(pos) = resp.hover_pos() {
                let f = (scroll/400.0).exp();
                let a = (pos - rect.left_top() - *pan) / *zoom;
                *zoom = (*zoom * f).clamp(Self::min_zoom(data), 6.0);
                *pan = pos - rect.left_top() - a * *zoom;
                *follow = false;
            }
        }

        // Follow — transform returns canvas map pixels. `pan` is relative
        // to rect.left_top(), so the centering vector must be too (using
        // the absolute rect.center() here left the dot offset down-right
        // by the toolbar height).
        if *follow {
            if let Some((x,_,z)) = player {
                let (ox, oy) = xf.apply(data.origin, x, z);
                *pan = (rect.center() - rect.left_top())
                    - egui::vec2(ox as f32, oy as f32) * *zoom;
            }
        }

        // Pan
        if resp.dragged() {
            *pan += resp.drag_delta();
            if *follow && resp.drag_delta().length() > 2.0 { *follow = false; }
        }

        // Calibration point: click marks the true spot for the player's
        // current world position; solving happens outside the canvas.
        if *calibrating && resp.clicked() {
            if let Some(click) = resp.interact_pointer_pos() {
                let mx = (click.x - rect.left_top().x - pan.x) as f64 / *zoom as f64;
                let my = (click.y - rect.left_top().y - pan.y) as f64 / *zoom as f64;
                *cal_action = Some(CalAction::AddPoint { mx, my });
                *calibrating = false;
                *follow = true;
            }
        }

        // ── Backdrop: stitched low-res overview ──
        // The base (surface) map dims while a floor layer is active —
        // like HoYoLab — so the full-brightness floor overlay stands out.
        let base_tint = if active_floor.is_empty() {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(150, 150, 150)
        };
        let map_screen = egui::vec2(
            data.total_size.0 as f32 * *zoom,
            data.total_size.1 as f32 * *zoom);
        painter.image(overview.id(),
            egui::Rect::from_min_size(rect.left_top()+*pan, map_screen),
            egui::Rect::from_min_max(egui::pos2(0.,0.), egui::pos2(1.,1.)),
            base_tint);

        // ── Sharp tiles streamed on demand ──
        let z = ((*zoom as f64).log2().floor() as i32).clamp(data.min_zoom, data.max_zoom);
        let tile_map = tiles::TILE_PX / 2f64.powi(z);   // map px per tile
        let tile_screen = (tile_map * *zoom as f64) as f32;
        let (cols, rows) = tiles::grid_for(data.total_size, z);
        // visible map-px range
        let mx0 = (-pan.x as f64 / *zoom as f64).max(0.0);
        let my0 = (-pan.y as f64 / *zoom as f64).max(0.0);
        let mx1 = ((rect.width() as f64 - pan.x as f64) / *zoom as f64).min(data.total_size.0);
        let my1 = ((rect.height() as f64 - pan.y as f64) / *zoom as f64).min(data.total_size.1);
        let tx0 = ((mx0 / tile_map).floor() as u32).min(cols.saturating_sub(1));
        let ty0 = ((my0 / tile_map).floor() as u32).min(rows.saturating_sub(1));
        let tx1 = ((mx1 / tile_map).ceil() as u32).min(cols.saturating_sub(1));
        let ty1 = ((my1 / tile_map).ceil() as u32).min(rows.saturating_sub(1));
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                if let Some(tex_id) = store.get(data.map_id, &data.version, z, tx, ty) {
                    let min = rect.left_top() + *pan
                        + egui::vec2(tx as f32 * tile_map as f32 * *zoom,
                                     ty as f32 * tile_map as f32 * *zoom);
                    painter.image(tex_id,
                        egui::Rect::from_min_size(min, egui::vec2(tile_screen, tile_screen)),
                        egui::Rect::from_min_max(egui::pos2(0.,0.), egui::pos2(1.,1.)),
                        base_tint);
                }
            }
        }

        // ── Floor (layer) overlay stack ──
        // NOTE: floor rects are in RAW pin space; the canvas works in
        // canvas space (raw + origin). Shallow stack members draw first
        // (dimmed), the active layer last (on top, full brightness) —
        // like the in-game map.
        for (floor, tex_id, dimmed) in active_floor {
            let Some(tex_id) = tex_id else { continue };
            let (rl, rt) = (
                floor.rect.0 + data.origin.0,
                floor.rect.1 + data.origin.1,
            );
            let (rr, rb) = (
                floor.rect.2 + data.origin.0,
                floor.rect.3 + data.origin.1,
            );
            let min = rect.left_top() + *pan
                + egui::vec2(rl as f32 * *zoom, rt as f32 * *zoom);
            let sz = egui::vec2(
                (rr - rl) as f32 * *zoom,
                (rb - rt) as f32 * *zoom);
            painter.image(*tex_id,
                egui::Rect::from_min_size(min, sz),
                egui::Rect::from_min_max(egui::pos2(0.,0.), egui::pos2(1.,1.)),
                if *dimmed {
                    egui::Color32::from_rgb(140, 140, 140)
                } else {
                    egui::Color32::WHITE
                });
        }

        // Screen helper for map-pixel positions.
        let to_screen = |mx: f64, my: f64| -> egui::Pos2 {
            rect.left_top() + *pan + egui::vec2(mx as f32 * *zoom, my as f32 * *zoom)
        };

        // ── API Pins ──
        if let Some(pd) = pin_data {
            let vis = pd.index.query(mx0, my0, mx1.max(mx0+1.0), my1.max(my0+1.0));
            let cluster = *zoom < 2f32.powi(data.min_zoom);
            // Icons grow with zoom (like the official map).
            let isize = (10.0 + 18.0 * zoom.sqrt()).clamp(14.0, 40.0);
            let mut clusters: HashMap<(i32,i32),(usize,egui::Pos2)> = HashMap::new();
            let mut drawn = 0; let mut total_vis = 0;
            // Candidates for click hit-testing.
            let mut hit: Option<(f32, u64, u32, egui::Pos2)> = None; // (dist², id, label, pos)

            // Pin visibility: pins are fully opaque or hidden — dimming
            // is reserved for map imagery (base map + shallow overlays).
            // Floor views show ONLY the active floor's pins; the surface
            // view shows only surface pins. The flatten toggle disables
            // layer partitioning entirely (everything, everywhere,
            // fully opaque).
            let active_ids: Option<&std::collections::HashSet<u64>> =
                active_floor
                    .iter()
                    .find(|(_, _, dimmed)| !dimmed)
                    .map(|(f, _, _)| &f.point_ids);

            for idx in vis {
                let pin = &pd.pins[idx];
                if !filter.is_enabled(pin.label_id) { continue; }
                let in_any_floor = pd.floors.iter()
                    .any(|f| f.point_ids.contains(&pin.id));
                if !filter.flatten_layers {
                    if active_floor.is_empty() {
                        // Surface view: surface pins only.
                        if in_any_floor { continue; }
                    } else {
                        // Floor view: only the active floor's pins —
                        // surface and other layers' pins are hidden.
                        if !active_ids.is_some_and(|s| s.contains(&pin.id)) {
                            continue;
                        }
                    }
                }
                let done = completed.contains(&pin.id);
                if done && filter.hide_completed { continue; }
                total_vis += 1;
                if drawn > 2500 { break; }

                let screen = to_screen(pin.x, pin.y);
                if !rect.contains(screen) { continue; }

                if cluster {
                    let c = ((screen.x/50.0) as i32, (screen.y/50.0) as i32);
                    clusters.entry(c).or_insert((0,screen)).0 += 1;
                } else {
                    // Track nearest clickable pin under the cursor.
                    if let Some(mouse) = resp.hover_pos() {
                        let d = screen.distance_sq(mouse);
                        let reach = (isize * 0.6 + 4.0).powi(2);
                        if d <= reach {
                            if hit.as_ref().map(|(bd, ..)| d < *bd).unwrap_or(true) {
                                hit = Some((d, pin.id, pin.label_id, screen));
                            }
                        }
                    }
                    let tint = if done {
                        egui::Color32::from_rgba_unmultiplied(255,255,255,90)
                    } else {
                        egui::Color32::WHITE
                    };
                    if let Some(icon_tex) = icons.get(&pin.label_id) {
                        painter.image(icon_tex.id(),
                            egui::Rect::from_center_size(screen, egui::vec2(isize, isize)),
                            egui::Rect::from_min_max(egui::pos2(0.,0.), egui::pos2(1.,1.)),
                            tint);
                    } else {
                        painter.circle_filled(screen, isize * 0.28,
                            if done {
                                egui::Color32::from_rgba_unmultiplied(160,160,160,90)
                            } else {
                                egui::Color32::from_rgba_unmultiplied(160,160,160,220)
                            });
                    }
                    drawn += 1;
                }
            }

            // Hover highlight + name tooltip.
            if let Some((_, _, hlid, hpos)) = &hit {
                painter.circle_stroke(*hpos, isize * 0.62,
                    egui::Stroke::new(2.0, egui::Color32::from_rgba_unmultiplied(255,255,255,200)));
                if let Some(label) = pd.labels.iter().find(|l| l.id == *hlid) {
                    painter.text(*hpos + egui::vec2(0.0, -(isize * 0.62) - 6.0),
                        egui::Align2::CENTER_BOTTOM, &label.name,
                        egui::FontId::proportional(12.0), egui::Color32::WHITE);
                }
            }

            // Click → select pin (empty click closes popup; calibrating takes priority).
            if resp.clicked() && !*calibrating {
                if let Some((_, id, label, _)) = hit {
                    *selected = Some(SelectedPin {
                        map_id: data.map_id, pin_id: id, label_id: label,
                    });
                } else {
                    *selected = None;
                }
            }

            // Right-click → instant mark/unmark toggle.
            if resp.secondary_clicked() && !*calibrating {
                if let Some((_, id, label, _)) = hit {
                    *quick_toggle = Some((id, label));
                }
            }

            for (_,(count,pos)) in &clusters {
                if *count > 1 {
                    painter.circle_filled(*pos, 9.0, egui::Color32::from_rgba_unmultiplied(70,130,220,200));
                    painter.circle_stroke(*pos, 9.0, egui::Stroke::new(1.5, egui::Color32::WHITE));
                    painter.text(*pos, egui::Align2::CENTER_CENTER, count.to_string(),
                        egui::FontId::proportional(10.0), egui::Color32::WHITE);
                } else {
                    painter.circle_filled(*pos, 4.0, egui::Color32::from_rgba_unmultiplied(70,130,220,220));
                }
            }

            let done_shown = if filter.hide_completed { 0 } else {
                pd.pins.iter().filter(|p| completed.contains(&p.id)).count()
            };
            painter.text(rect.left_bottom()+egui::vec2(8.,-24.), egui::Align2::LEFT_BOTTOM,
                format!("{} — pins: {} visible, {} drawn, {} collected",
                    data.name, total_vis, drawn, done_shown),
                egui::FontId::monospace(10.0),
                egui::Color32::from_rgba_unmultiplied(255,255,255,180));
        }

        // ── Auto-collect toasts (top-right, fading) ──
        let now = std::time::Instant::now();
        for (i, (txt, t)) in auto_notes.iter().enumerate() {
            let age = now.duration_since(*t).as_secs_f32();
            if age >= 4.0 { continue; }
            let alpha = (((4.0 - age) / 1.0).clamp(0.0, 1.0) * 230.0) as u8;
            let pos = egui::pos2(rect.right_top().x - 8.0, rect.right_top().y + 34.0 + i as f32 * 17.0);
            painter.text(pos, egui::Align2::RIGHT_TOP, txt,
                egui::FontId::proportional(12.0),
                egui::Color32::from_rgba_unmultiplied(120, 255, 150, alpha));
        }

        // ── Player dot ──
        if let Some((x,_,z)) = player {
            let (mx, my) = xf.apply(data.origin, x, z);
            let p = to_screen(mx, my);
            if rect.contains(p) {
                painter.circle_filled(p, 7.0, egui::Color32::from_rgb(70,230,120));
                painter.circle_stroke(p, 10.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgba_unmultiplied(70,230,120,120)));
            }
        }

        // ── Uncalibrated-layer banner (big, centered, unmissable) ──
        if cal_info.2 {
            let c = rect.center();
            painter.text(
                c + egui::vec2(0.0, -60.0),
                egui::Align2::CENTER_CENTER,
                "⚠ This layer is not calibrated",
                egui::FontId::proportional(20.0),
                egui::Color32::from_rgb(240, 200, 80),
            );
            painter.text(
                c + egui::vec2(0.0, -30.0),
                egui::Align2::CENTER_CENTER,
                "pick this floor in the sidebar → 📍+ cal → click your spot",
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgba_unmultiplied(255,255,255,220),
            );
        }
    }

    fn min_zoom(data: &MapData) -> f32 {
        2f32.powi(data.min_zoom - 1).max(0.005)
    }
}
