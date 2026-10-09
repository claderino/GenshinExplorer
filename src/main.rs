//! GenshinExplorer — live world tracking from game network traffic.
//!
//! Reuses the GenshinReader capture/decrypt stack and applies value-pattern
//! detection to exploration data: player movement, gadget (chest) spawns
//! and interactions. Phase 1: live dashboard + learning logs.

mod admin;
mod capture;
mod cmd_names;
mod cookies;
mod explore;
mod map;
mod mapwin;
mod mora;
mod pins;
mod proto_walk;
mod reasons;
mod sync;
mod tiles;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use eframe::egui;
use explore::{EntityObs, Interact, Motion};

#[derive(Debug)]
pub enum Msg {
    Info(String),
    Error(String),
    Tracking(bool),
    Motion(Motion),
    Entity(EntityObs),
    Interact(Interact),
    Mora { amount: i64, reason: String },
    Chest { amount: i64, x: f32, y: f32, z: f32, kind: String },
    /// Authoritative player position from the continuity-filtered tracker.
    Position { x: f32, y: f32, z: f32 },
    /// In-game UID identified from avatar-scene votes.
    Uid(u32),
    /// Oculus gadget config id seen in a small command (see
    /// explore::OCULUS_IDS — Anemo..Cryo from the 7.1 item table)
    /// — the server-side state change when the nearby oculus is collected.
    Oculus { x: f32, z: f32 },
    /// Challenge completed (20234-shape success) — player position included.
    ChallengeDone { x: f32, z: f32 },
    /// Region-feature broadcast identified the active map (6771 shape).
    Region(u32),
    /// Scene change detected (big enter-scene packet) — bumps the scene
    /// generation used for calibration-frame discrimination.
    SceneChanged,
    /// Teleport arrival — used for floor (layer) auto-selection via
    /// nearest-pin ownership matching.
    TeleportArrival { x: f32, y: f32, z: f32, dy: f32 },
    /// Minimap layer entry (5991 _EnterMapLayerReq) — the definitive
    /// floor signal. `None` = default (base) layer.
    MapLayer { layer_id: Option<u64> },
}

fn main() -> Result<()> {
    // Packet capture needs elevation.
    if !admin::is_elevated() {
        println!("Requesting administrator rights (needed for packet capture)...");
        admin::relaunch_elevated()?;
        return Ok(());
    }

    let (tx, rx) = channel::<Msg>();
    std::thread::Builder::new()
        .name("explorer-capture".into())
        .spawn(move || {
            if let Err(e) = worker_main(tx.clone()) {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
            }
        })
        .context("spawning capture worker")?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([700.0, 450.0])
            .with_title("GenshinExplorer — world tracker"),
        ..Default::default()
    };
    eframe::run_native(
        "GenshinExplorer",
        options,
        Box::new(move |cc| {
            install_emoji_font(&cc.egui_ctx);
            Ok(Box::new(ExplorerApp::new(rx)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

/// egui's bundled fonts have no emoji coverage — most of the UI's
/// pictographs (📍 🗺 🗂 🧲 …) would render as tofu boxes. Register the
/// monochrome Noto Emoji as a fallback for both font families.
/// (Color emoji fonts aren't supported by epaint's rasterizer.)
fn install_emoji_font(ctx: &egui::Context) {
    const NOTO_EMOJI: &[u8] =
        include_bytes!("../assets/NotoEmoji-Regular.ttf");
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto-emoji".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(NOTO_EMOJI)),
    );
    for family in [
        egui::FontFamily::Proportional,
        egui::FontFamily::Monospace,
    ] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("noto-emoji".to_owned());
    }
    ctx.set_fonts(fonts);
}

fn load_keys() -> Result<HashMap<u16, Vec<u8>>> {
    const KEYS_JSON: &[u8] = include_bytes!("../keys/gi.json");
    let encoded: HashMap<u16, String> =
        serde_json::from_slice(KEYS_JSON).context("parsing embedded dispatch keys")?;
    let mut keys = HashMap::new();
    for (id, b64) in encoded {
        keys.insert(id, base64::prelude::BASE64_STANDARD.decode(b64)?);
    }
    Ok(keys)
}

fn worker_main(tx: Sender<Msg>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("creating worker runtime")?;

    rt.block_on(async move {
        let keys = load_keys()?;
        let mut sniffer = auto_artifactarium::GameSniffer::new().set_initial_keys(keys);

        let _ = tx.send(Msg::Info(
            "Listening on UDP 22101/22102 — cold-start the game to begin tracking".into(),
        ));

        let mut frames = capture::start_capture(false)?;
        let mut tracking = false;

        // Gadget entity registry (for interact matching).
        let mut gadget_entities: HashMap<u64, ()> = HashMap::new();
        // Command census + small-command capture (pattern learning).
        let mut census: HashMap<u16, (u64, Option<String>)> = HashMap::new();
        let mut last_census = std::time::Instant::now();
        let mut small_logged: usize = 0;
        // Recent small commands (ring buffer) for Mora correlation — a
        // chest's interact request lands within ~1 s of the Mora gain.
        let mut recent_small: Vec<(std::time::Instant, u16, String)> = Vec::new();
        // World-position vector streams (per carrying command id).
        // (worker side: superseded by the PositionTracker; the UI keeps its
        // own copy fed by Msg::Motion)
        // Continuity-filtered player position tracker.
        let mut pos_tracker = explore::PositionTracker::new();
        // Player's entity_id for batch filtering (learned from the direct
        // carrier's position matching a batch entry).
        let mut player_entity_id: Option<u64> = None;
        // Avatar-scene UID votes → identifies the in-game account (7.x
        // removed user_id from wire headers).
        let mut uid_votes: HashMap<u32, u32> = HashMap::new();
        let mut detected_uid: Option<u32> = None;
        let mut chests_log_pending = true;
        let _ = chests_log_pending;
        // Position log (aggregated, 2 s per entity).
        let data_dir = PathBuf::from(
            std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join("GenshinExplorer"),
        );
        std::fs::create_dir_all(&data_dir)?;
        let mut positions_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("positions.jsonl"))?;
        let mut entities_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("entities.jsonl"))?;
        let mut interacts_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("interacts.jsonl"))?;
        let mut census_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("census.jsonl"))?;
        let mut smallcmds_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("smallcmds.jsonl"))?;
        let mut chest_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("chest_candidates.jsonl"))?;
        // Gadget-reward shaped packets (652/25131 shapes) — uncapped ground
        // truth for chest/oculus rewards.
        let mut gadget_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("gadgetrewards.jsonl"))?;
        // Scene-transition capture: everything for 2.5 s after a teleport.
        let mut scene_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("scenetransitions.jsonl"))?;
        let mut scene_capture_until: Option<std::time::Instant> = None;
        let mut scene_event: u64 = 0;
        let mut last_sent_pos: Option<(f32, f32, f32)> = None;
        // Region-feature tracking (6771 broadcasts) → active region map.
        let mut region_feature_at: Option<(String, std::time::Instant)> = None;
        let mut active_region: Option<u32> = None;
        let mut first_position_at: Option<std::time::Instant> = None;
        // Scene-id tracking (9582 PlayerEnterSceneNotify) — the
        // authoritative "which world am I in" signal. Separate regions
        // (Chasm, moon, Enkanomiya, …) each have their own scene and
        // their own local coordinate system; cross-map geometry is
        // meaningless between them.
        let mut current_scene: Option<u64> = None;
        let mut seen_unknown_scenes: Vec<u64> = Vec::new();
        // Big packets (>1 KB) only fire on real scene changes (enter-scene
        // notifies) — distinguishes the Chasm underground (scene change)
        // from its surface (same scene as Teyvat).
        let mut last_big_packet: Option<std::time::Instant> = None;
        let mut last_scene_notify: Option<std::time::Instant> = None;
        // Position flow = game actively running. The world pauses while the
        // in-game map is open, which also pauses region broadcasts — the
        // Teyvat fallback must not fire then. The direct carrier keeps
        // reporting a FROZEN position while paused, so liveness alone is
        // not enough: the fallback also requires the position to have
        // actually changed recently (last_pos_change_at).
        let mut last_position_at: Option<std::time::Instant> = None;
        let mut last_pos_change_at: Option<std::time::Instant> = None;
        let mut last_chest_send: Option<std::time::Instant> = None;
        // Armed by WorldChestOpenNotify (28616); resolved by the ItemAdd
        // chest path, or sent as a Mora-less fallback after 2.5 s.
        let mut pending_world_chest: Option<(
            (f32, f32, f32),
            std::time::Instant,
        )> = None;
        // Full research capture: ALL decrypted commands, any size, first
        // 1KB of hex. No count limit — for targeted analysis sessions.
        let mut fullcap_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("fullcap.jsonl"))?;
        let mut fullcap_count: u64 = 0;
        // Token-exchange dump: the 7.1 new-format handshake is not
        // decryptable with the current dispatch keys — persist the FULL
        // Req/Rsp payloads (untruncated) so the key exchange can be
        // analyzed offline.
        let mut token_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("token_exchange.jsonl"))?;

        let mut last_pos_write: HashMap<u64, std::time::Instant> = HashMap::new();

        loop {
            let Some(frame) = frames.recv().await else {
                anyhow::bail!("capture channel closed");
            };
            match sniffer.receive_packet(frame) {
                Some(auto_artifactarium::GamePacket::Connection(
                    auto_artifactarium::ConnectionPacket::HandshakeRequested,
                )) => {
                    tracking = false;
                    gadget_entities.clear();
                    let _ = tx.send(Msg::Tracking(false));
                    let _ = tx.send(Msg::Info("New login handshake".into()));
                }
                Some(auto_artifactarium::GamePacket::Commands(commands)) => {
                    if !tracking && !commands.is_empty() {
                        tracking = true;
                        let _ = tx.send(Msg::Tracking(true));
                        let _ = tx.send(Msg::Info("Tracking — decrypting game traffic".into()));
                    }
                    for command in &commands {
                        // WorldChestOpenNotify fallback resolution: armed by
                        // the dedicated chest-open packet, normally resolved
                        // within milliseconds by the ItemAdd chest path
                        // (which knows the Mora amount). If no ItemAdd
                        // arrives, the chest still gets marked.
                        if let Some((pos, since)) = pending_world_chest {
                            if since.elapsed()
                                >= Duration::from_millis(2500)
                            {
                                pending_world_chest = None;
                                let (x, y, z) = pos;
                                {
                                    use std::io::Write;
                                    let _ = writeln!(
                                        chest_log,
                                        "{}",
                                        serde_json::json!({
                                            "chest": true,
                                            "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                            "amount": 0,
                                            "type_estimate": "world_chest_notify",
                                            "pos": [x, y, z],
                                        })
                                    );
                                }
                                let _ = tx.send(Msg::Chest {
                                    amount: 0,
                                    x,
                                    y,
                                    z,
                                    kind: "common".into(),
                                });
                                last_chest_send =
                                    Some(std::time::Instant::now());
                            }
                        }

                        // Command census: counts + a hex sample per id, so
                        // real packet shapes can be derived offline.
                        {
                            use std::io::Write;
                            let entry = census.entry(command.command_id).or_insert((0u64, None));
                            entry.0 += 1;
                            if entry.1.is_none() {
                                let sample: String = command
                                    .proto_data
                                    .iter()
                                    .take(512)
                                    .map(|b| format!("{b:02x}"))
                                    .collect();
                                entry.1 = Some(sample);
                            }
                            if last_census.elapsed() >= Duration::from_secs(10) {
                                last_census = std::time::Instant::now();
                                let mut counts: Vec<_> = census.iter().collect();
                                counts.sort_by_key(|(_, e)| std::cmp::Reverse(e.0));
                                let rows: Vec<serde_json::Value> = counts
                                    .iter()
                                    .take(40)
                                    .map(|(id, (n, sample))| {
                                        serde_json::json!({
                                            "command_id": id,
                                            "count": n,
                                            "sample_hex": sample,
                                        })
                                    })
                                    .collect();
                                let _ = writeln!(
                                    census_log,
                                    "{}",
                                    serde_json::json!({
                                        "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                        "top": rows,
                                    })
                                );
                            }
                        }

                        // Small-command log: interact requests live here
                        // (correlated with Mora gains below).
                        if command.proto_data.len() <= 128 && small_logged < 200_000 {
                            use std::io::Write;
                            small_logged += 1;
                            let hex: String = command
                                .proto_data
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect();
                            recent_small.push((
                                std::time::Instant::now(),
                                command.command_id,
                                hex.clone(),
                            ));
                            if recent_small.len() > 256 {
                                let drop = recent_small.len() - 256;
                                recent_small.drain(0..drop);
                            }
                            let _ = writeln!(
                                smallcmds_log,
                                "{}",
                                serde_json::json!({
                                    "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                    "command_id": command.command_id,
                                    "len": command.proto_data.len(),
                                    "hex": hex,
                                })
                            );
                        }

                        // Very large packets (>3 KB) = true enter-scene
                        // notifies (~3.7 KB observed) — waypoint teleports
                        // also emit >1 KB packets and must NOT bump the
                        // scene generation.
                        if command.proto_data.len() > 3000 {
                            let now = std::time::Instant::now();
                            // Debounce: one notification per burst.
                            let is_new = last_scene_notify
                                .map(|t| now.duration_since(t)
                                    >= Duration::from_secs(5))
                                .unwrap_or(true);
                            if is_new {
                                last_scene_notify = Some(now);
                                let _ = tx.send(Msg::SceneChanged);
                                // Layers are separate coordinate frames —
                                // accept the next position immediately.
                                pos_tracker.reset();
                            }
                            last_big_packet = Some(now);
                        } else if command.proto_data.len() > 1000 {
                            last_big_packet = Some(std::time::Instant::now());
                        }

                        // _EnterMapLayerReq/Rsp (5991/21115): the client
                        // reports the map layer the minimap switched to
                        // when crossing a world-area boundary. The Req
                        // payload is `{4: map_layer_id}` (empty = default
                        // layer) — the definitive floor signal. The Rsp
                        // is a bare retcode and carries nothing.
                        if command.command_id == 5991 {
                            if let Some(layer_id) =
                                explore::detect_map_layer(&command.proto_data)
                            {
                                tracing::debug!(
                                    "map layer → {layer_id:?}"
                                );
                                let _ =
                                    tx.send(Msg::MapLayer { layer_id });
                            }
                        }

                        // Named entity notifications — entity type names as
                        // readable strings (spawn/despawn lifecycle).
                        if let Some(ne) = explore::detect_named_entity(command) {
                            tracing::debug!(
                                "entity: {} (id={}, action={})",
                                ne.name, ne.entity_id, ne.action
                            );
                        }

                        // Scene entry (9582): the authoritative world
                        // discriminator — each separate region has its
                        // own scene id and local coordinates. Known ids
                        // drive the map directly; unknown ones are
                        // logged once so they can be mapped later.
                        if command.command_id == 9582 {
                            if let Some((scene, prev)) =
                                explore::detect_scene_enter(
                                    &command.proto_data,
                                )
                            {
                                tracing::debug!(
                                    scene,
                                    prev,
                                    "scene enter"
                                );
                                current_scene = Some(scene);
                                let mapped = match scene {
                                    429_4906_403 => Some(2u32),  // Teyvat
                                    429_4906_400 => Some(9u32),  // Chasm mines
                                    429_4906_496 => Some(40u32), // moon
                                    _ => None,
                                };
                                if let Some(map_id) = mapped {
                                    if active_region != Some(map_id) {
                                        active_region = Some(map_id);
                                        let _ =
                                            tx.send(Msg::Region(map_id));
                                    }
                                } else if !seen_unknown_scenes
                                    .contains(&scene)
                                {
                                    seen_unknown_scenes.push(scene);
                                    tracing::info!(
                                        scene,
                                        "unknown scene id — visit-logged \
                                         for region mapping"
                                    );
                                }
                            }
                        }

                        // Region features (6771 = RegionalPlayInfoNotify):
                        // periodic broadcast naming the special region the
                        // player is inside. Stragglers (field 9 absent) are
                        // sent once on *leaving* a region — ignored so they
                        // can't switch the map or refresh the silence timer.
                        if command.proto_data.len() <= 96 {
                            if let Some(rf) =
                                explore::region_feature(&command.proto_data)
                            {
                                if !rf.periodic {
                                    tracing::debug!(
                                        "region straggler '{}' — ignoring",
                                        rf.name
                                    );
                                } else {
                                    let scene_change = last_big_packet
                                        .map(|t| {
                                            t.elapsed()
                                                < Duration::from_secs(15)
                                        })
                                        .unwrap_or(false);
                                    let map_id = match rf.name.as_str() {
                                        // Chasm: only the underground mines
                                        // are a separate scene — the surface
                                        // shares Teyvat's scene and must
                                        // stay on map 2.
                                        "LightStone" if scene_change => {
                                            Some(9u32)
                                        }
                                        "LightStone" => {
                                            tracing::debug!(
                                                "LightStone (surface) — \
                                                 staying on Teyvat"
                                            );
                                            None
                                        }
                                        // Nod-Krai is its own scene; every
                                        // entry is a scene change anyway.
                                        "MoonFatigue" => Some(40u32),
                                        _ => {
                                            tracing::info!(
                                                "unknown region feature: {}",
                                                rf.name
                                            );
                                            None
                                        }
                                    };
                                    region_feature_at = Some((
                                        rf.name,
                                        std::time::Instant::now(),
                                    ));
                                    if let Some(id) = map_id {
                                        if active_region != Some(id) {
                                            active_region = Some(id);
                                            let _ =
                                                tx.send(Msg::Region(id));
                                        }
                                    }
                                }
                            }
                        }
                        // Broadcast silence → back on the main map. Only
                        // while the game is actively running (position
                        // updates flowing). Also fires when active_region
                        // is None (app restart with no region seen yet):
                        // a 6 s grace from the first position fix lets a
                        // sub-map's periodic broadcast (~1 Hz, e.g.
                        // MoonFatigue) arrive before defaulting to the
                        // surface map.
                        let needs_surface = match active_region {
                            Some(r) => r != 2,
                            None => true, // default to surface when unknown
                        }
                            // Only flip to Teyvat from the Teyvat scene
                            // itself (or before any scene was seen) —
                            // unknown separate regions (Enkanomiya etc.)
                            // must not be dragged to the surface map.
                            && current_scene
                                .map(|s| s == 429_4906_403)
                                .unwrap_or(true);
                        let broadcast_silent = region_feature_at
                            .as_ref()
                            .map(|(_, t)| {
                                // Periodic broadcasts repeat at 1–2 Hz, so
                                // 8 s of silence while positions keep
                                // flowing means the region was left.
                                t.elapsed() >= Duration::from_secs(8)
                            })
                            .unwrap_or_else(|| {
                                first_position_at
                                    .map(|t| {
                                        t.elapsed()
                                            >= Duration::from_secs(6)
                                    })
                                    .unwrap_or(false)
                            });
                        if needs_surface
                            && broadcast_silent
                            && last_position_at
                                .map(|t| {
                                    t.elapsed() < Duration::from_secs(8)
                                })
                                .unwrap_or(false)
                            && last_pos_change_at
                                .map(|t| {
                                    // World actually running — a frozen
                                    // position (in-game map open, world
                                    // paused) must not trigger the
                                    // fallback; 60 s is generous enough
                                    // to cover a brief stop after really
                                    // leaving a region.
                                    t.elapsed()
                                        < Duration::from_secs(60)
                                })
                                .unwrap_or(false)
                        {
                            if active_region != Some(2) {
                                active_region = Some(2);
                                let _ = tx.send(Msg::Region(2));
                            }
                        }

                        // Scene-transition capture window: log every command.
                        if let Some(until) = scene_capture_until {
                            if std::time::Instant::now() < until {
                                use std::io::Write;
                                let hex: String = command
                                    .proto_data
                                    .iter()
                                    .take(256)
                                    .map(|b| format!("{b:02x}"))
                                    .collect();
                                let _ = writeln!(
                                    scene_log,
                                    "{}",
                                    serde_json::json!({
                                        "event": scene_event,
                                        "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                        "command_id": command.command_id,
                                        "len": command.proto_data.len(),
                                        "hex": hex,
                                    })
                                );
                            } else {
                                scene_capture_until = None;
                            }
                        }

                        // Full research capture: log every command.
                        {
                            use std::io::Write;
                            fullcap_count += 1;
                            let hex: String = command
                                .proto_data
                                .iter()
                                .take(512) // 512 bytes = 1024 hex chars
                                .map(|b| format!("{b:02x}"))
                                .collect();
                            let _ = writeln!(
                                fullcap_log,
                                "{}",
                                serde_json::json!({
                                    "n": fullcap_count,
                                    "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                    "cmd": command.command_id,
                                    "name": cmd_names::command_name(command.command_id),
                                    "len": command.proto_data.len(),
                                    "hex": hex,
                                })
                            );
                        }

                        // Token exchange (GetPlayerTokenReq/Rsp): dump the
                        // FULL untruncated payload — the new-format 7.1
                        // handshake (~19.9 KB Rsp vs the old ~25.5 KB) is
                        // undecryptable with current keys and needs offline
                        // analysis of its key blobs.
                        if command.command_id == 23252 || command.command_id == 3713 {
                            use std::io::Write;
                            let hex: String = command
                                .proto_data
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect();
                            let _ = writeln!(
                                token_log,
                                "{}",
                                serde_json::json!({
                                    "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                    "cmd": command.command_id,
                                    "name": cmd_names::command_name(command.command_id),
                                    "len": command.proto_data.len(),
                                    "hex": hex,
                                })
                            );
                        }

                        // UID voting (avatar-scene) — identifies the account in-game.
                        if detected_uid.is_none() {
                            mora::detect_uid_candidates(command, &mut uid_votes);
                            if let Some((&best, &count)) =
                                uid_votes.iter().max_by_key(|(_, c)| **c)
                            {
                                if count >= 3 {
                                    detected_uid = Some(best);
                                    let _ = tx.send(Msg::Uid(best));
                                }
                            }
                        }

                        // Mora gains + chest correlation.
                        for event in mora::detect_mora_events(command) {
                            // Chest opens: chest-family ItemAdd (39 OPEN_CHEST,
                            // 52 OPEN_WORLD_BOSS_CHEST, 55 OPEN_BLOSSOM_CHEST)
                            // fires for ANY item content — including chests
                            // that give no Mora (count 0).
                            if let mora::MoraEvent::ItemAdd {
                                action_reason: Some(r),
                                count,
                                ..
                            } = &event
                            {
                                if matches!(r, 39 | 52 | 55)
                                    && last_chest_send
                                        .map(|t| t.elapsed() >= Duration::from_millis(4000))
                                        .unwrap_or(true)
                                {
                                    let amount = i64::try_from(*count).unwrap_or(0);
                                    // Authoritative player position (the
                                    // continuity-filtered tracker) — the
                                    // player stands at the chest when it
                                    // opens.
                                    let pos = pos_tracker.last;
                                    let chest_type = if amount >= 1200 {
                                        "precious"
                                    } else if amount >= 500 {
                                        "exquisite"
                                    } else {
                                        "common"
                                    };
                                    if let Some((x, y, z)) = pos {
                                        {
                                            use std::io::Write;
                                            let _ = writeln!(
                                                chest_log,
                                                "{}",
                                                serde_json::json!({
                                                    "chest": true,
                                                    "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                                    "amount": amount,
                                                    "type_estimate": chest_type,
                                                    "pos": pos,
                                                })
                                            );
                                        }
                                        let _ = tx.send(Msg::Chest {
                                            amount,
                                            x,
                                            y,
                                            z,
                                            kind: chest_type.to_string(),
                                        });
                                        last_chest_send = Some(std::time::Instant::now());
                                        // Satisfied by the ItemAdd path —
                                        // cancel the 28616 fallback.
                                        pending_world_chest = None;
                                    }
                                }
                            }

                            if let Some(amount) = event.delta().filter(|a| *a > 0) {
                                let reason = event.reason_label();
                                // Correlate recent small commands (±1.5 s).
                                let now = std::time::Instant::now();
                                let correlated: Vec<serde_json::Value> = recent_small
                                    .iter()
                                    .filter(|(t, _, _)| now.duration_since(*t).as_millis() <= 1500)
                                    .map(|(t, id, hex)| {
                                        serde_json::json!({
                                            "command_id": id,
                                            "hex": hex,
                                            "delta_ms": now.duration_since(*t).as_millis() as u64,
                                        })
                                    })
                                    .collect();
                                {
                                    use std::io::Write;
                                    let _ = writeln!(
                                        chest_log,
                                        "{}",
                                        serde_json::json!({
                                            "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                            "amount": amount,
                                            "reason": reason,
                                            "correlated": correlated,
                                        })
                                    );
                                }
                                let _ = tx.send(Msg::Mora { amount, reason });
                            }
                        }

                        // Oculus collection: the server-side gadget state
                        // change carries an oculus config id (see
                        // explore::OCULUS_IDS — the full 7.1 item-table
                        // family) in small commands. Position matching
                        // against uncollected oculus pins happens in the map.
                        if command.proto_data.len() <= 64
                            && explore::contains_oculus_config(&command.proto_data)
                        {
                            if let Some((px, _py, pz)) =
                                pos_tracker.last
                            {
                                let _ = tx.send(Msg::Oculus { x: px, z: pz });
                            }
                        }

                        // WorldChestOpenNotify (28616): the dedicated
                        // chest-open broadcast `{2: scene_id, 11:
                        // config_id, 12: group_id}` — no position (the
                        // player stands at the chest), no ActionReason
                        // dependency. Arms a pending mark; the ItemAdd
                        // path (which knows the Mora amount) normally
                        // resolves it within milliseconds.
                        if command.command_id == 28616 {
                            if last_chest_send
                                .map(|t| {
                                    t.elapsed()
                                        >= Duration::from_millis(4000)
                                })
                                .unwrap_or(true)
                            {
                                if let Some((x, y, z)) = pos_tracker.last {
                                    tracing::debug!(
                                        "world chest open — armed pending \
                                         mark at player position"
                                    );
                                    pending_world_chest = Some((
                                        (x, y, z),
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        }

                        // Challenge result (20234-shape): field 10 = 2 means
                        // completed — mark the challenge pin instantly at the
                        // player's position (they are at the challenge).
                        if let Some(result) = explore::detect_challenge_result(command) {
                            tracing::info!(
                                "challenge {} ({}s / goal {})",
                                if result.success { "SUCCESS" } else { "failed" },
                                result.seconds, result.goal
                            );
                            if result.success {
                                if let Some((px, _py, pz)) = pos_tracker.last {
                                    let _ = tx.send(Msg::ChallengeDone { x: px, z: pz });
                                }
                            }
                        }

                        // Gadget-reward shaped packets (652: {11: reason,
                        // 13: {item, count}}, 25131: {pos, 15: id-varint}).
                        // NOTE: these are state BROADCASTS — they fire on
                        // unlock/approach (e.g. camp chest unlocks during
                        // combat), NOT on collection. Chest collection is
                        // detected only via ItemAddNotify (items actually
                        // granted) in the mora-event loop above. Logged here
                        // for research; only oculus ids trigger (verified to
                        // fire only on collection).
                        if let Some(gr) = explore::detect_gadget_reward(command) {
                            {
                                use std::io::Write;
                                let _ = writeln!(
                                    gadget_log,
                                    "{}",
                                    serde_json::json!({
                                        "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                        "command_id": gr.command_id,
                                        "reason": gr.action_reason,
                                        "id": gr.id,
                                        "count": gr.count,
                                        "pos": gr.pos.map(|(x, y, z)| vec![x, y, z]),
                                    })
                                );
                            }
                            // Oculus ids fire only on collection — exact
                            // packet position supplements the byte-scan
                            // path; without a packet position (some
                            // variants carry none) the player's current
                            // position is a sound fallback — you stand
                            // at the oculus when collecting it.
                            if explore::is_oculus_id(gr.id) {
                                let target = gr
                                    .pos
                                    .map(|(x, _y, z)| (x, z))
                                    .or_else(|| {
                                        pos_tracker
                                            .last
                                            .map(|(px, _py, pz)| (px, pz))
                                    });
                                if let Some((x, z)) = target {
                                    let _ = tx.send(Msg::Oculus { x, z });
                                }
                            }
                        }

                        // Player position: direct carrier (ground truth, ~every 6s)
                        // + batch carrier filtered by learned entity_id (~1Hz).
                        if let Some(motion) = explore::detect_player_position(command) {
                            if pos_tracker.accept(motion.x, motion.y, motion.z) {
                                let (px, py, pz) = pos_tracker.last.unwrap();
                                last_position_at = Some(std::time::Instant::now());
                                if first_position_at.is_none() {
                                    first_position_at =
                                        Some(std::time::Instant::now());
                                }
                                // First position fix of the session (game
                                // login, or the app starting mid-session):
                                // there is no previous position, so no
                                // teleport can be detected — but the
                                // arrival still deserves floor selection.
                                // Emit a synthetic arrival so the waypoint
                                // pin-match picks the floor of the
                                // teleporter you spawned next to.
                                if last_sent_pos.is_none() {
                                    let _ = tx.send(Msg::TeleportArrival {
                                        x: px, y: py, z: pz, dy: 0.0,
                                    });
                                }
                                // Liveness for the region fallback: the
                                // direct carrier reports a frozen position
                                // while the in-game map is open (world
                                // paused) — only a real change proves the
                                // world is running.
                                let moved = last_sent_pos
                                    .map(|(lx, ly, lz)| {
                                        (px - lx).abs() > 0.5
                                            || (py - ly).abs() > 0.5
                                            || (pz - lz).abs() > 0.5
                                    })
                                    .unwrap_or(true);
                                if moved {
                                    last_pos_change_at =
                                        Some(std::time::Instant::now());
                                }
                                // Teleport: a big x-z jump OR a big height
                                // jump. Layered teleports (stacked floors)
                                // can land within a few units in x-z while
                                // moving hundreds of units in y — the y
                                // delta is the discriminator. 250 stays
                                // clear of sustained dive-fall speeds
                                // between position samples.
                                if let Some((lx, ly, lz)) = last_sent_pos {
                                    let d = ((px - lx).powi(2) + (pz - lz).powi(2)).sqrt();
                                    let dy = (py - ly).abs();
                                    if d > 200.0 || dy > 250.0 {
                                        scene_capture_until =
                                            Some(std::time::Instant::now()
                                                + Duration::from_millis(2500));
                                        scene_event += 1;
                                        // Any teleport resets continuity —
                                        // layers are separate coordinate
                                        // frames and jumps exceed the
                                        // filter's limits.
                                        pos_tracker.reset();
                                        let _ = tx.send(
                                            Msg::TeleportArrival {
                                                x: px, y: py, z: pz, dy,
                                            });
                                        {
                                            use std::io::Write;
                                            let _ = writeln!(
                                                scene_log,
                                                "{}",
                                                serde_json::json!({
                                                    "event": scene_event,
                                                    "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                                    "teleport": true,
                                                    "from": [lx, ly, lz],
                                                    "to": [px, py, pz],
                                                })
                                            );
                                        }
                                        let _ = tx.send(Msg::Info(format!(
                                            "teleport detected ({d:.0} units, Δy {dy:.0}) — capturing scene packets"
                                        )));
                                    }
                                }
                                last_sent_pos = Some((px, py, pz));
                                let _ = tx.send(Msg::Position { x: px, y: py, z: pz });
                                {
                                    use std::io::Write;
                                    let due = last_pos_write
                                        .get(&0u64)
                                        .map(|t| t.elapsed() >= Duration::from_secs(2))
                                        .unwrap_or(true);
                                    if due {
                                        last_pos_write.insert(0, std::time::Instant::now());
                                        let _ = writeln!(positions_log, "{}", serde_json::json!({
                                            "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                            "x": px, "y": py, "z": pz,
                                        }));
                                    }
                                }
                            }
                        }

                        // Batch entries: learn player entity_id, then use
                        // their entries for faster (~1 Hz) updates.
                        for (entity_id, motion) in explore::detect_batch_entries(command) {
                            match player_entity_id {
                                None => {
                                    // Learning phase: find the entity whose
                                    // position is closest to the last direct
                                    // carrier position.
                                    if let Some((lx, _ly, lz)) = pos_tracker.last {
                                        let dist = ((motion.x - lx).powi(2)
                                            + (motion.z - lz).powi(2))
                                        .sqrt();
                                        if dist < 30.0 {
                                            player_entity_id = Some(entity_id);
                                            tracing::info!(
                                                entity_id,
                                                "learned player entity_id from batch"
                                            );
                                        }
                                    }
                                }
                                Some(pid) if entity_id == pid => {
                                    // Player's batch entry — faster update.
                                    if pos_tracker.accept(motion.x, motion.y, motion.z) {
                                        let (px, py, pz) = pos_tracker.last.unwrap();
                                        let _ = tx.send(Msg::Position { x: px, y: py, z: pz });
                                    }
                                }
                                _ => {}
                            }
                        }
                        for entity in explore::detect_entities(command) {
                            for &(_, id) in &entity.varints {
                                if id > 10_000_000 {
                                    // Register large ids as candidate gadget entities.
                                    let _ = gadget_entities.entry(id).or_insert(());
                                }
                            }
                            {
                                use std::io::Write;
                                let varints: serde_json::Map<String, serde_json::Value> =
                                    entity
                                        .varints
                                        .iter()
                                        .map(|(f, v)| (format!("f{f}"), (*v).into()))
                                        .collect();
                                let row = serde_json::json!({
                                    "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                    "command_id": entity.command_id,
                                    "varints": varints,
                                    "x": entity.x, "y": entity.y, "z": entity.z,
                                });
                                let _ = writeln!(entities_log, "{row}");
                            }
                            let _ = tx.send(Msg::Entity(entity));
                        }
                        if let Some(interact) = explore::detect_interact(command, &gadget_entities) {
                            {
                                use std::io::Write;
                                let row = serde_json::json!({
                                    "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                    "entity": interact.entity_id,
                                    "command_id": interact.command_id,
                                });
                                let _ = writeln!(interacts_log, "{row}");
                            }
                            let _ = tx.send(Msg::Interact(interact));
                        }
                    }
                }
                _ => {}
            }
        }
    })
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TrackedEntity {
    last_motion: Motion,
    updates: u64,
    last_seen: std::time::Instant,
}

struct ExplorerApp {
    rx: Receiver<Msg>,
    tracking: bool,
    status: String,
    error: Option<String>,
    entities: HashMap<u64, TrackedEntity>,
    vector_streams: HashMap<u16, (u64, Motion)>,
    feed: Vec<String>, // retained (kept small); no longer rendered
    player_entity: Option<u64>,
    map_window: mapwin::MapWindow,
    chests: Vec<mapwin::ChestMark>,
    /// In-game UID (avatar-vote detected) — namespaces map state.
    uid: Option<u32>,
    /// Authoritative player position from the continuity tracker.
    player_pos: Option<(f32, f32, f32)>,
}

/// Chest history from previous sessions (chest_candidates.jsonl rows with
/// `chest: true` and a position).
fn load_chest_history() -> Vec<mapwin::ChestMark> {
    let path = std::path::PathBuf::from(
        std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .join("GenshinExplorer")
            .join("chest_candidates.jsonl"),
    );
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if row.get("chest").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            let Some(pos) = row.get("pos").and_then(|v| v.as_array()) else {
                continue;
            };
            let (Some(x), Some(y), Some(z)) = (
                pos.first().and_then(|v| v.as_f64()),
                pos.get(1).and_then(|v| v.as_f64()),
                pos.get(2).and_then(|v| v.as_f64()),
            ) else {
                continue;
            };
            out.push(mapwin::ChestMark {
                x: x as f32,
                y: y as f32,
                z: z as f32,
                amount: row.get("amount").and_then(|v| v.as_i64()).unwrap_or(0),
                kind: row
                    .get("type_estimate")
                    .and_then(|v| v.as_str())
                    .unwrap_or("common")
                    .to_string(),
            });
        }
    }
    out
}

impl ExplorerApp {
    fn new(rx: Receiver<Msg>) -> Self {
        Self {
            rx,
            tracking: false,
            status: "Starting…".into(),
            error: None,
            entities: HashMap::new(),
            vector_streams: HashMap::new(),
            feed: Vec::new(),
            player_entity: None,
            map_window: mapwin::MapWindow::new(),
            chests: load_chest_history(),
            player_pos: None,
            uid: None,
        }
    }

    fn push_feed(&mut self, line: String) {
        tracing::debug!("{line}");
        self.feed.push(line);
        if self.feed.len() > 400 {
            let drop = self.feed.len() - 400;
            self.feed.drain(0..drop);
        }
    }

    fn player_pos(&self) -> Option<(f32, f32, f32)> {
        self.player_pos
    }
}

impl eframe::App for ExplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Info(text) => self.status = text,
                Msg::Error(text) => self.error = Some(text),
                Msg::Tracking(ok) => self.tracking = ok,
                Msg::Position { x, y, z } => {
                    self.player_pos = Some((x, y, z));
                }
                Msg::Motion(m) => {
                    let entry = self
                        .entities
                        .entry(m.entity_id)
                        .or_insert_with(|| TrackedEntity {
                            last_motion: m.clone(),
                            updates: 0,
                            last_seen: std::time::Instant::now(),
                        });
                    entry.last_motion = m.clone();
                    entry.updates += 1;
                    entry.last_seen = std::time::Instant::now();
                }
                Msg::Entity(e) => {
                    let ids: Vec<String> = e
                        .varints
                        .iter()
                        .map(|(f, v)| format!("f{f}={v}"))
                        .collect();
                    self.push_feed(format!(
                        "spawn  [{:<6}] {} @ ({:.0}, {:.0}, {:.0})",
                        e.command_id,
                        ids.join(" "),
                        e.x, e.y, e.z
                    ));
                }
                Msg::Interact(i) => {
                    let pos = self
                        .entities
                        .get(&i.entity_id)
                        .map(|t| {
                            format!(
                                " @ ({:.0}, {:.0}, {:.0})",
                                t.last_motion.x, t.last_motion.y, t.last_motion.z
                            )
                        })
                        .unwrap_or_default();
                    self.push_feed(format!("INTERACT [{}] entity {}{}", i.command_id, i.entity_id, pos));
                }
                Msg::Mora { amount, reason } => {
                    self.push_feed(format!("+{amount} Mora ({reason})"));
                }
                Msg::Chest { amount, x, y, z, kind } => {
                    tracing::info!(
                        "CHEST +{amount} Mora [{kind}] @ ({x:.0}, {y:.0}, {z:.0}) — stored, queued for auto-collect");
                    self.chests.push(mapwin::ChestMark { x, y, z, amount, kind: kind.clone() });
                    self.map_window.note_chest(mapwin::ChestMark { x, y, z, amount, kind });
                }
                Msg::Uid(uid) => {
                    tracing::info!("in-game UID {uid} identified");
                    self.uid = Some(uid);
                }
                Msg::Oculus { x, z } => {
                    self.map_window.note_oculus(x, z);
                }
                Msg::ChallengeDone { x, z } => {
                    self.map_window.note_challenge_done(x, z);
                }
                Msg::Region(map_id) => {
                    self.map_window.note_region(map_id);
                }
                Msg::SceneChanged => {
                    self.map_window.scene_gen += 1;
                }
                Msg::TeleportArrival { x, y, z, dy } => {
                    self.map_window.note_teleport(x, y, z, dy);
                }
                Msg::MapLayer { layer_id } => {
                    self.map_window.note_map_layer(layer_id);
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(250));

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let (color, label) = if let Some(err) = &self.error {
                    (egui::Color32::from_rgb(230, 110, 110), format!("● error: {}", err.lines().next().unwrap_or("")))
                } else if self.tracking {
                    (egui::Color32::from_rgb(110, 210, 130), "● tracking".to_string())
                } else {
                    (egui::Color32::from_rgb(230, 190, 90), "● waiting for login".to_string())
                };
                ui.colored_label(color, label);
                if let Some(uid) = self.uid {
                    ui.separator();
                    ui.strong(format!("UID {uid}"));
                }
                ui.separator();

                // Player = the world-position stream that updates most.
                if let Some((_, entry)) = self
                    .vector_streams
                    .iter()
                    .max_by_key(|(_, entry)| entry.0)
                {
                    let m = &entry.1;
                    ui.strong(format!(
                        "pos x={:.1} y={:.1} z={:.1}  (stream updates: {})",
                        m.x, m.y, m.z, entry.0
                    ));
                } else {
                    ui.weak("no world-position stream yet");
                }
            });
            ui.add_space(2.0);
            ui.small(egui::RichText::new(&self.status).weak());
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("entities").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("entities (most active)");
                if ui.button(if self.map_window.open { "🗺 hide map" } else { "🗺 Map" }).clicked() {
                    self.map_window.open = !self.map_window.open;
                }
            });
            let mut rows: Vec<(&u64, &TrackedEntity)> = self
                .entities
                .iter()
                .filter(|(_, t)| t.last_seen.elapsed() < Duration::from_secs(60))
                .collect();
            rows.sort_by_key(|(_, t)| std::cmp::Reverse(t.updates));
            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                egui::Grid::new("entities").striped(true).show(ui, |ui| {
                    ui.strong("entity");
                    ui.strong("updates");
                    ui.strong("x");
                    ui.strong("y");
                    ui.strong("z");
                    ui.end_row();
                    for (id, t) in rows.iter().take(14) {
                        ui.monospace(format!("{id}"));
                        ui.monospace(format!("{}", t.updates));
                        ui.monospace(format!("{:.1}", t.last_motion.x));
                        ui.monospace(format!("{:.1}", t.last_motion.y));
                        ui.monospace(format!("{:.1}", t.last_motion.z));
                        ui.end_row();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.map_window.open {
                let player = self.player_pos();
                let dir = std::env::var_os("LOCALAPPDATA")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default()
                    .join("GenshinExplorer")
                    .join("map");
                self.map_window.show_docked(ui, player, &dir, self.uid);
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.weak("map hidden — press 🗺 Map below");
                });
            }
        });
    }
}

