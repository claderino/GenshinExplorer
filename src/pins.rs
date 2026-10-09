//! Pin data with individual label toggles, icon downloads, multi-map
//! support, and HoYoLab's own label-tree grouping (exact sidebar parity).
//!
//! Detection semantics are kept separate from display grouping:
//!  * display groups come from the `/v2/map/label/tree` API (parent_id)
//!  * `PinCategory` is the small semantic set used by the packet matchers
//!    (chest opens, oculus collects, challenge completions)

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eframe::egui;

const PIN_API: &str =
    "https://sg-public-api-static.hoyolab.com/common/map_user/ys_obc/v3/map/point/list";
const MAP_INFO_API: &str =
    "https://sg-public-api-static.hoyolab.com/common/map_user/ys_obc/v3/map/info";
const LABEL_TREE_API: &str =
    "https://sg-public-api-static.hoyolab.com/common/map_user/ys_obc/v2/map/label/tree";

// ---------------------------------------------------------------------------
// Detection semantics (packet matchers)
// ---------------------------------------------------------------------------

/// Semantic pin families used by the packet-detection matchers. NOT the
/// sidebar grouping — that comes from the API label tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinCategory {
    /// True chest types (HoYoLab group "Chests", id 13).
    Chests,
    /// All eight *oculus labels (under "Special Items").
    Oculi,
    /// The three challenge families: Time Trial (64), Warrior's (600),
    /// Totem (647).
    Challenges,
}

/// HoYoLab label ids for the challenge families.
pub const CHALLENGE_LABELS: [u32; 3] = [64, 600, 647];

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Pin {
    pub id: u64,
    pub label_id: u32,
    pub x: f64, // original map pixel coords
    pub y: f64,
}

#[derive(Debug, Clone)]
pub struct LabelEntry {
    pub id: u32,
    pub name: String,
    /// HoYoLab top-level group id (from the label tree).
    pub parent_id: u32,
    pub pin_count: usize,
    pub icon_url: String,
    /// Decoded icon image (created on background thread, converted to texture on UI thread).
    pub icon_image: Option<egui::ColorImage>,
}

/// A sidebar display group — mirrors HoYoLab's own grouping.
#[derive(Debug, Clone)]
pub struct GroupInfo {
    pub id: u32,
    pub name: String,
    /// Indices into PinData::labels.
    pub label_indices: Vec<usize>,
}

#[derive(Clone)]
pub struct MapInfo {
    pub map_id: u32,
    pub name: String,
    pub origin: (f64, f64),
    pub total_size: (f64, f64),
    /// detail_v2 maps report pin coords relative to origin (can be negative).
    pub origin_relative: bool,
}

impl MapInfo {
    /// From the v3 info API body (shared cache file with map.rs).
    pub fn from_info_body(json: &serde_json::Value, map_id: u32) -> Option<MapInfo> {
        let name = json.pointer("/data/info/name")?.as_str()?.to_string();
        let arr2 = |v: &serde_json::Value, key: &str| -> Option<(f64, f64)> {
            let a = v.get(key)?.as_array()?;
            Some((a[0].as_f64()?, a[1].as_f64()?))
        };
        // Prefer the detail_v2 pyramid (all current maps have it).
        if let Some(v2) = json.pointer("/data/info/detail_v2").filter(|v| !v.is_null()) {
            return Some(MapInfo {
                map_id,
                name,
                origin: arr2(v2, "origin")?,
                total_size: arr2(v2, "total_size")?,
                origin_relative: true,
            });
        }
        // Fallback: slice scheme.
        let detail = json.pointer("/data/info/detail")?.as_str()?;
        let detail: serde_json::Value = serde_json::from_str(detail).ok()?;
        Some(MapInfo {
            map_id,
            name,
            origin: arr2(&detail, "origin")?,
            total_size: arr2(&detail, "total_size")?,
            origin_relative: false,
        })
    }
}

/// A map floor (layer) from the point-group API: an overlay image placed
/// as a rect on the canvas, plus the pins belonging to this floor.
#[derive(Debug, Clone)]
pub struct FloorInfo {
    pub group_id: u32,
    pub floor_id: u32,
    pub group_name: String,
    pub name: String,
    pub overlay_url: String,
    /// Canvas rect (l_x, l_y, r_x, r_y).
    pub rect: (f64, f64, f64, f64),
    pub size: (u32, u32),
    pub point_ids: std::collections::HashSet<u64>,
}

pub struct PinData {
    pub pins: Vec<Pin>,
    /// Labels sorted by group (tree order) then pin count.
    pub labels: Vec<LabelEntry>,
    /// HoYoLab sidebar groups (tree order), containing labels with pins.
    pub groups: Vec<GroupInfo>,
    /// Layered floors (empty for flat maps).
    pub floors: Vec<FloorInfo>,
    pub index: PinIndex,
    pub map_info: MapInfo,
    /// pin id → label id (for completion stats per label).
    pub pin_label: HashMap<u64, u32>,
}

impl PinData {
    /// Label ids belonging to a detection-semantic family.
    pub fn semantic_labels(&self, cat: PinCategory) -> std::collections::HashSet<u32> {
        match cat {
            PinCategory::Chests => self.labels.iter()
                .filter(|l| l.parent_id == 13)
                .map(|l| l.id).collect(),
            PinCategory::Oculi => self.labels.iter()
                .filter(|l| l.name.to_lowercase().contains("oculus"))
                .map(|l| l.id).collect(),
            PinCategory::Challenges => self.labels.iter()
                .filter(|l| CHALLENGE_LABELS.contains(&l.id))
                .map(|l| l.id).collect(),
        }
    }
}

pub struct PinIndex {
    cell_size: f64,
    cells: HashMap<(i32, i32), Vec<usize>>,
}

impl PinIndex {
    pub fn new(pins: &[Pin], cell_size: f64) -> Self {
        let mut cells: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (i, p) in pins.iter().enumerate() {
            cells.entry((p.x as i32 / cell_size as i32, p.y as i32 / cell_size as i32))
                .or_default().push(i);
        }
        Self { cell_size, cells }
    }
    pub fn query(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<usize> {
        let c0 = (x0 / self.cell_size) as i32; let r0 = (y0 / self.cell_size) as i32;
        let c1 = (x1 / self.cell_size) as i32; let r1 = (y1 / self.cell_size) as i32;
        let mut out = Vec::new();
        for c in c0..=c1 { for r in r0..=r1 {
            if let Some(v) = self.cells.get(&(c, r)) { out.extend_from_slice(v); }
        }}
        out
    }
}

// ---------------------------------------------------------------------------
// Loading (pins + labels + label tree + icons + map info)
// ---------------------------------------------------------------------------

pub fn load_pin_data(map_id: u32, cache_dir: &Path) -> Result<PinData> {
    std::fs::create_dir_all(cache_dir)?;
    let cache = cache_dir.join(format!("pins_v3_{}.json", map_id));
    let body = if cache.exists() {
        std::fs::read_to_string(&cache)?
    } else {
        let url = format!("{}?map_id={}&app_sn=ys_obc&lang=en-us", PIN_API, map_id);
        let agent = ureq::AgentBuilder::new().user_agent("Mozilla/5.0").build();
        let mut reader = agent.get(&url).call().context("downloading pins")?.into_reader();
        let mut buf = String::new();
        reader.read_to_string(&mut buf)?;
        std::fs::write(&cache, &buf)?;
        buf
    };

    let json: serde_json::Value = serde_json::from_str(&body)?;
    parse(&json, map_id, cache_dir)
}

fn parse(json: &serde_json::Value, map_id: u32, cache_dir: &Path) -> Result<PinData> {
    let points = json.pointer("/data/point_list").and_then(|v| v.as_array()).context("point_list")?;
    let labels_raw = json.pointer("/data/label_list").and_then(|v| v.as_array()).context("label_list")?;

    // Label tree → label id → top-level group (id, name), any depth.
    let tree = load_label_tree(map_id, cache_dir).unwrap_or_default();
    let mut label_group: HashMap<u32, (u32, String)> = HashMap::new();
    for group in &tree {
        let gid = group.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let gname = group.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        collect_group_ids(group, gid, &gname, &mut label_group);
    }

    // Build label entries.
    let mut label_entries: Vec<LabelEntry> = labels_raw.iter().filter_map(|l| {
        let id = l.get("id")?.as_u64()? as u32;
        let name = l.get("name")?.as_str()?.to_string();
        let icon = l.get("icon").and_then(|v| v.as_str()).unwrap_or("");
        let parent_id = l.get("parent_id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        Some(LabelEntry {
            id,
            name: name.clone(),
            parent_id,
            pin_count: 0,
            icon_url: icon.to_string(),
            icon_image: None,
        })
    }).collect();

    // Parse pins.
    let pins: Vec<Pin> = points.iter().filter_map(|p| {
        Some(Pin {
            id: p.get("id")?.as_u64()?,
            label_id: p.get("label_id")?.as_u64()? as u32,
            x: p.get("x_pos")?.as_f64()?,
            y: p.get("y_pos")?.as_f64()?,
        })
    }).collect();

    // Count pins per label, keep only labels that have pins.
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for p in &pins { *counts.entry(p.label_id).or_default() += 1; }
    for l in &mut label_entries {
        l.pin_count = counts.get(&l.id).copied().unwrap_or(0);
    }
    label_entries.retain(|l| l.pin_count > 0);

    // Resolve each label's DISPLAY group via the tree (fall back: a flat
    // "Pins" group keyed by the raw parent id).
    let mut group_order: Vec<(u32, String)> = tree.iter().filter_map(|g| {
        let id = g.get("id").and_then(|v| v.as_u64())? as u32;
        let name = g.get("name").and_then(|v| v.as_str())?.to_string();
        Some((id, name))
    }).collect();
    let mut seen_groups: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, l) in label_entries.iter().enumerate() {
        let (gid, gname) = label_group.get(&l.parent_id)
            .cloned()
            .unwrap_or_else(|| (l.parent_id, format!("Group {}", l.parent_id)));
        if !group_order.iter().any(|(id, _)| *id == gid) {
            group_order.push((gid, gname));
        }
        seen_groups.entry(gid).or_default().push(i);
    }

    // Sort labels: by group (tree order), then count descending.
    let group_pos = |gid: u32| group_order.iter().position(|(id, _)| *id == gid).unwrap_or(999);
    label_entries.sort_by(|a, b| {
        let ga = group_pos(label_group.get(&a.parent_id).map(|(g, _)| *g).unwrap_or(a.parent_id));
        let gb = group_pos(label_group.get(&b.parent_id).map(|(g, _)| *g).unwrap_or(b.parent_id));
        ga.cmp(&gb).then(b.pin_count.cmp(&a.pin_count))
    });

    // Groups in tree order with their label indices (post-sort).
    let mut groups: Vec<GroupInfo> = Vec::new();
    for (gid, gname) in &group_order {
        let idxs: Vec<usize> = label_entries.iter().enumerate()
            .filter(|(_, l)| {
                let resolved = label_group.get(&l.parent_id).map(|(g, _)| *g).unwrap_or(l.parent_id);
                resolved == *gid
            })
            .map(|(i, _)| i)
            .collect();
        if !idxs.is_empty() {
            groups.push(GroupInfo { id: *gid, name: gname.clone(), label_indices: idxs });
        }
    }

    // Map info + pin normalization.
    let map_info = load_map_info(map_id, cache_dir).unwrap_or(MapInfo {
        map_id,
        name: "Unknown".into(),
        origin: (12524.0, 7406.0),
        total_size: (22528.0, 20480.0),
        origin_relative: false,
    });
    let mut pins = pins;
    if map_info.origin_relative {
        for p in &mut pins {
            p.x += map_info.origin.0;
            p.y += map_info.origin.1;
        }
    }

    // Icons.
    let icon_cache_dir = cache_dir.join("icons");
    let _ = std::fs::create_dir_all(&icon_cache_dir);
    for label in &mut label_entries {
        if !label.icon_url.is_empty() {
            label.icon_image = load_icon(&label.icon_url, &icon_cache_dir);
        }
    }

    let index = PinIndex::new(&pins, 256.0);
    let pin_label: HashMap<u64, u32> = pins.iter().map(|p| (p.id, p.label_id)).collect();
    let floors = load_point_groups(map_id, cache_dir);

    tracing::info!(
        map_id, pins = pins.len(), labels = label_entries.len(),
        groups = groups.len(), floors = floors.len(),
        "pin data loaded (API label-tree grouping)"
    );

    Ok(PinData { pins, labels: label_entries, groups, floors, index, map_info, pin_label })
}

/// Recursively map every node id in a label-tree group to its top-level
/// (group id, group name).
fn collect_group_ids(
    node: &serde_json::Value,
    group_id: u32,
    group_name: &str,
    out: &mut HashMap<u32, (u32, String)>,
) {
    let Some(id) = node.get("id").and_then(|v| v.as_u64()) else { return };
    out.insert(id as u32, (group_id, group_name.to_string()));
    if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
        for c in children {
            collect_group_ids(c, group_id, group_name, out);
        }
    }
}

/// Label tree: `data.tree[] = {id, name, children…}` — cached per map.
fn load_label_tree(map_id: u32, cache_dir: &Path) -> Option<Vec<serde_json::Value>> {
    let cache = cache_dir.join(format!("label_tree_v2_{}.json", map_id));
    let body = if cache.exists() {
        std::fs::read_to_string(&cache).ok()?
    } else {
        let url = format!("{LABEL_TREE_API}?app_sn=ys_obc&lang=en-us&map_id={map_id}");
        let agent = ureq::AgentBuilder::new().user_agent("Mozilla/5.0").build();
        let mut reader = agent.get(&url).call().ok()?.into_reader();
        let mut buf = String::new();
        reader.read_to_string(&mut buf).ok()?;
        std::fs::write(&cache, &buf).ok()?;
        buf
    };
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    json.pointer("/data/tree")
        .and_then(|v| v.as_array())
        .cloned()
}

/// Point groups (layered floors) per map — cached.
fn load_point_groups(map_id: u32, cache_dir: &Path) -> Vec<FloorInfo> {
    let cache = cache_dir.join(format!("point_groups_{map_id}.json"));
    let body = if cache.exists() {
        std::fs::read_to_string(&cache).ok()
    } else {
        let fetched = (|| -> Option<String> {
            let url = format!(
                "https://sg-public-api-static.hoyolab.com/common/map_user/ys_obc\
                 /v2/map/point_group?map_id={map_id}&app_sn=ys_obc&lang=en-us"
            );
            let agent =
                ureq::AgentBuilder::new().user_agent("Mozilla/5.0").build();
            let mut reader =
                agent.get(&url).call().ok()?.into_reader();
            let mut buf = String::new();
            reader.read_to_string(&mut buf).ok()?;
            std::fs::write(&cache, &buf).ok()?;
            Some(buf)
        })();
        fetched
    };
    let Some(body) = body else { return Vec::new() };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
        return Vec::new();
    };
    let Some(groups) = json.pointer("/data/list").and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    let mut floors = Vec::new();
    for g in groups {
        let group_id = g.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let group_name = g
            .get("g_floor_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let Some(fs) = g.get("floors").and_then(|v| v.as_array()) else {
            continue;
        };
        for f in fs {
            let floor_id =
                f.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let name = f
                .get("floor_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let overlay = f
                .pointer("/overlay/url")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if overlay.is_empty() {
                continue;
            }
            let num = |k: &str| {
                f.pointer(&format!("/overlay/{k}"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            };
            let rect = (num("l_x"), num("l_y"), num("r_x"), num("r_y"));
            let size = (
                f.pointer("/overlay/width")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                f.pointer("/overlay/height")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
            );
            let point_ids: std::collections::HashSet<u64> = f
                .get("point_ids")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|p| p.as_u64()).collect())
                .unwrap_or_default();
            floors.push(FloorInfo {
                group_id,
                floor_id,
                group_name: group_name.clone(),
                name,
                overlay_url: overlay.to_string(),
                rect,
                size,
                point_ids,
            });
        }
    }
    tracing::info!(map_id, floors = floors.len(), "point groups loaded");
    floors
}

fn load_map_info(map_id: u32, cache_dir: &Path) -> Option<MapInfo> {
    let cache = cache_dir.join(format!("info_v3_{}.json", map_id));
    let body = if cache.exists() {
        std::fs::read_to_string(&cache).ok()?
    } else {
        let url = format!("{}?map_id={}&app_sn=ys_obc&lang=en-us", MAP_INFO_API, map_id);
        let agent = ureq::AgentBuilder::new().user_agent("Mozilla/5.0").build();
        let mut reader = agent.get(&url).call().ok()?.into_reader();
        let mut buf = String::new();
        reader.read_to_string(&mut buf).ok()?;
        std::fs::write(&cache, &buf).ok()?;
        buf
    };
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    MapInfo::from_info_body(&json, map_id)
}

fn load_icon(url: &str, cache_dir: &Path) -> Option<egui::ColorImage> {
    let hash = url.split('/').last()?.split('_').last()?.split('.').next()?;
    let cache_file = cache_dir.join(format!("{}.png", hash));

    let png_data = if cache_file.exists() {
        std::fs::read(&cache_file).ok()?
    } else {
        let agent = ureq::AgentBuilder::new().user_agent("Mozilla/5.0").build();
        let mut reader = agent.get(url).call().ok()?.into_reader();
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).ok()?;
        std::fs::write(&cache_file, &buf).ok()?;
        buf
    };

    let img = image::load_from_memory(&png_data).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &rgba,
    ))
}

// ---------------------------------------------------------------------------
// Available maps
// ---------------------------------------------------------------------------

pub fn available_maps() -> Vec<(u32, &'static str)> {
    // English names exactly as served by the international v3 map/info API.
    vec![
        (2, "Teyvat"),
        (7, "Enkanomiya"),
        (9, "The Chasm: Underground Mines"),
        (34, "Sea of Bygone Eras"),
        (36, "Ancient Sacred Mountain"),
        (37, "Temple of Space"),
        (40, "Frost Moon"),
    ]
}

// ---------------------------------------------------------------------------
// Filter state (individual label toggles)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct FilterState {
    pub enabled_labels: std::collections::HashSet<u32>,
    /// When true, collected pins are hidden entirely; otherwise they fade.
    pub hide_completed: bool,
}

impl FilterState {
    pub fn is_enabled(&self, label_id: u32) -> bool {
        self.enabled_labels.contains(&label_id)
    }
    pub fn toggle(&mut self, label_id: u32) {
        if self.enabled_labels.contains(&label_id) {
            self.enabled_labels.remove(&label_id);
        } else {
            self.enabled_labels.insert(label_id);
        }
    }
    pub fn set_ids(&mut self, ids: &[u32], enable: bool) {
        for id in ids {
            if enable { self.enabled_labels.insert(*id); }
            else { self.enabled_labels.remove(id); }
        }
    }
    pub fn all_enabled(&self, ids: &[u32]) -> bool {
        ids.iter().all(|id| self.enabled_labels.contains(id))
    }
    pub fn any_enabled(&self, ids: &[u32]) -> bool {
        ids.iter().any(|id| self.enabled_labels.contains(id))
    }
    /// Defaults: HoYoLab's primary gameplay groups — Waypoints (1),
    /// Special Items (4, includes oculi), Chests (13), Puzzle Chest (186).
    pub fn with_defaults(data: &PinData) -> Self {
        let mut s = Self::default();
        for group in &data.groups {
            if matches!(group.id, 1 | 4 | 13 | 186) {
                for &li in &group.label_indices {
                    s.enabled_labels.insert(data.labels[li].id);
                }
            }
        }
        s
    }
    pub fn load(dir: &Path, uid: u32, map_id: u32) -> Self {
        // uid 0 = pre-UID files (back-compat filename without uid).
        let name = if uid == 0 {
            format!("pin_filters_v3_{map_id}.json")
        } else {
            format!("pin_filters_v3_{uid}_{map_id}.json")
        };
        std::fs::read_to_string(dir.join(name))
            .ok().and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    pub fn save(&self, dir: &Path, uid: u32, map_id: u32) {
        let name = if uid == 0 {
            format!("pin_filters_v3_{map_id}.json")
        } else {
            format!("pin_filters_v3_{uid}_{map_id}.json")
        };
        let _ = std::fs::write(
            dir.join(name),
            serde_json::to_string_pretty(self).unwrap_or_default(),
        );
    }
}
