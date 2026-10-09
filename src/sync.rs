//! HoYoLab sync — one-way import of officially marked ("collected") pins
//! from the account's map-user data.
//!
//! Endpoint (from the web map's JS): GET
//! `sg-public-api.hoyolab.com/common/map_user/ys_obc/v1/map/point/mark_map_point_list`
//! with the browser's Cookie header (any authenticated request's cookies
//! work; the user pastes theirs).

use std::io::Read;

use anyhow::{Context, Result, bail};

const MARKS_API: &str =
    "https://sg-public-api.hoyolab.com/common/map_user/ys_obc/v1/map/point/mark_map_point_list";

/// POST an add/delete for one point (shared request shape).
fn post_point(
    endpoint: &str,
    map_id: u32,
    point_id: u64,
    cookie: &str,
) -> Result<()> {
    let dev_id = cookie_value(cookie, "_HYVUUID")
        .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".into());
    let dev_fp = cookie_value(cookie, "DEVICEFP")
        .unwrap_or_else(|| "00000000000".into());
    let agent = ureq::AgentBuilder::new()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
        .build();
    let resp = agent
        .post(&format!("https://sg-public-api.hoyolab.com/common/map_user/ys_obc/v1/map/point/{endpoint}"))
        .set("Cookie", cookie.trim())
        .set("Referer", "https://act.hoyolab.com/")
        .set("Origin", "https://act.hoyolab.com")
        .set("Content-Type", "application/json;charset=utf-8")
        .set("x-rpc-map_version", "4.5")
        .set("x-rpc-device_id", &dev_id)
        .set("x-rpc-device_fp", &dev_fp)
        .set("x-rpc-platform", "4")
        .set("x-rpc-page", "v6.0.2__#/map/2")
        .set("x-rpc-view_source", "1")
        .send_json(serde_json::json!({
            "map_id": map_id,
            "point_id": point_id,
            "app_sn": "ys_obc",
            "lang": "en-us",
        }))
        .context("HoYoLab request failed")?;
    let mut body = String::new();
    resp.into_reader().read_to_string(&mut body)?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing response: {body}"))?;
    let retcode = json.get("retcode").and_then(|v| v.as_i64()).unwrap_or(-1);
    if retcode != 0 {
        let msg = json.get("message").and_then(|v| v.as_str()).unwrap_or("?");
        bail!("HoYoLab error {retcode}: {msg}");
    }
    Ok(())
}

/// Batch mark/unmark: `items` are `(point_id, is_delete)` — `is_delete`
/// false marks, true unmarks. Sent in chunks of 100.
pub fn batch_mark(
    map_id: u32,
    items: &[(u64, bool)],
    cookie: &str,
) -> Result<(usize, usize)> {
    let dev_id = cookie_value(cookie, "_HYVUUID")
        .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".into());
    let dev_fp = cookie_value(cookie, "DEVICEFP")
        .unwrap_or_else(|| "00000000000".into());
    let agent = ureq::AgentBuilder::new()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
        .build();

    let mut ok = 0usize;
    let mut failed = 0usize;
    for chunk in items.chunks(100) {
        let list: Vec<serde_json::Value> = chunk
            .iter()
            .map(|(id, del)| serde_json::json!({"point_id": id, "is_delete": del}))
            .collect();
        let res = agent
            .post("https://sg-public-api.hoyolab.com/common/map_user/ys_obc/v1/map/point/batch_mark_map_point")
            .set("Cookie", cookie.trim())
            .set("Referer", "https://act.hoyolab.com/")
            .set("Origin", "https://act.hoyolab.com")
            .set("Content-Type", "application/json;charset=utf-8")
            .set("x-rpc-map_version", "4.5")
            .set("x-rpc-device_id", &dev_id)
            .set("x-rpc-device_fp", &dev_fp)
            .set("x-rpc-platform", "4")
            .set("x-rpc-page", "v6.0.2__#/map/2")
            .set("x-rpc-view_source", "1")
            .send_json(serde_json::json!({
                "map_id": map_id,
                "list": list,
                "app_sn": "ys_obc",
                "lang": "en-us",
            }));
        match res {
            Ok(resp) if resp.status() == 200 => ok += chunk.len(),
            _ => failed += chunk.len(),
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    if failed > 0 {
        tracing::warn!(map_id, ok, failed, "batch_mark had failures");
    }
    Ok((ok, failed))
}

/// A game character bound to a HoYoLab account.
#[derive(Clone)]
pub struct GameRole {
    pub uid: u32,
    pub region: String,
    pub nickname: String,
}

pub fn region_display(region: &str) -> &'static str {
    match region {
        "os_usa" => "America",
        "os_eu" => "Europe",
        "os_asia" => "Asia",
        "os_cht" => "TW/HK/MO",
        _ => "Unknown",
    }
}

/// List the Genshin characters bound to a session, one per region
/// (`getUserGameRolesByLtoken`). Silent per-region failures → fewer roles.
pub fn fetch_game_roles(cookie: &str) -> Vec<GameRole> {
    let agent = ureq::AgentBuilder::new()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
        .build();
    let mut roles = Vec::new();
    for region in ["os_usa", "os_eu", "os_asia", "os_cht"] {
        let url = format!(
            "https://sg-public-api.hoyolab.com/binding/api/getUserGameRolesByLtoken\
             ?game_biz=hk4e_global&region={region}"
        );
        let Ok(resp) = agent
            .get(&url)
            .set("Cookie", cookie.trim())
            .set("x-rpc-language", "en")
            .set("Referer", "https://act.hoyolab.com/")
            .call()
        else {
            continue;
        };
        let mut body = String::new();
        if resp.into_reader().read_to_string(&mut body).is_err() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        let Some(list) = json.pointer("/data/list").and_then(|v| v.as_array()) else {
            continue;
        };
        for it in list {
            let (Some(uid), Some(nick)) = (
                it.get("game_uid").and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u32>().ok()),
                it.get("nickname").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            roles.push(GameRole {
                uid,
                region: region.to_string(),
                nickname: nick.to_string(),
            });
        }
    }
    roles
}

/// Mark one point on HoYoLab.
pub fn add_mark(map_id: u32, point_id: u64, cookie: &str) -> Result<()> {
    post_point("add_mark_map_point", map_id, point_id, cookie)
}

/// Unmark one point on HoYoLab.
pub fn delete_mark(map_id: u32, point_id: u64, cookie: &str) -> Result<()> {
    post_point("new_del_mark_map_point", map_id, point_id, cookie)
}

/// Push locally-collected pins that HoYoLab doesn't have yet (additive —
/// never deletes remote marks). Reports progress (done, total).
pub fn push_missing(
    map_id: u32,
    uid: u32,
    local: Vec<u64>,
    cookie: &str,
    progress: &dyn Fn(usize, usize),
) -> Result<(usize, usize)> {
    // Existing remote marks — skip them.
    let existing: std::collections::HashSet<u64> =
        fetch_marks(map_id, uid, cookie)?.point_ids.into_iter().collect();
    let to_push: Vec<u64> = local
        .into_iter()
        .filter(|id| !existing.contains(id))
        .collect();
    let total = to_push.len();
    if total == 0 {
        return Ok((0, 0));
    }

    // Device identity from the cookie (writes require the x-rpc headers).
    // Batch endpoint handles chunks internally.
    let items: Vec<(u64, bool)> = to_push.iter().map(|&id| (id, false)).collect();
    progress(0, total);
    let (ok, failed) = batch_mark(map_id, &items, cookie)?;
    progress(total, total);
    tracing::info!(map_id, uid, pushed = ok, failed, "HoYoLab push complete");
    Ok((ok, failed))
}

/// Extract a cookie value (`name=value`) from a raw Cookie header string.
fn cookie_value(cookie: &str, name: &str) -> Option<String> {
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}
/// Genshin UID first digit → region code.
fn region_for_uid(uid: u32) -> &'static str {
    match uid / 100_000_000 {
        6 => "os_usa",
        7 => "os_eu",
        8 => "os_asia",
        9 => "os_cht",
        _ => "os_usa",
    }
}

pub struct MarksResult {
    /// API point ids that are marked.
    pub point_ids: Vec<u64>,
    /// Total marks returned (before filtering).
    pub total: usize,
    /// Region that authenticated.
    pub region: String,
}

fn marks_request(map_id: u32, uid: u32, region: &str, cookie: &str) -> Result<serde_json::Value> {
    let url = format!(
        "{MARKS_API}?app_sn=ys_obc&lang=en-us&map_id={map_id}&uid={uid}&region={region}"
    );
    let agent = ureq::AgentBuilder::new()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36")
        .build();
    let resp = agent
        .get(&url)
        .set("Cookie", cookie.trim())
        .set("Referer", "https://act.hoyolab.com/")
        .set("Origin", "https://act.hoyolab.com")
        .set("x-rpc-map_version", "4.5")
        .set("x-rpc-platform", "4")
        .set("x-rpc-view_source", "1")
        .call()
        .context("HoYoLab request failed (check cookie)")?;
    let mut body = String::new();
    resp.into_reader().read_to_string(&mut body)?;
    let json: serde_json::Value =
        serde_json::from_str(&body).context("parsing HoYoLab response")?;
    let retcode = json.get("retcode").and_then(|v| v.as_i64()).unwrap_or(-1);
    if retcode != 0 {
        let msg = json.get("message").and_then(|v| v.as_str()).unwrap_or("?");
        bail!("HoYoLab error {retcode}: {msg}");
    }
    Ok(json)
}

/// Fetch the account's marked points for a map. `cookie` is the raw Cookie
/// header from an authenticated hoyolab request; the region is derived from
/// the UID (with fallback to trying every region).
pub fn fetch_marks(map_id: u32, uid: u32, cookie: &str) -> Result<MarksResult> {
    let primary = region_for_uid(uid);
    let mut orders: Vec<&str> = vec![primary];
    for r in ["os_usa", "os_eu", "os_asia", "os_cht"] {
        if r != primary { orders.push(r); }
    }

    let mut json = None;
    let mut used = String::new();
    let mut last_err: Option<anyhow::Error> = None;
    for region in orders {
        match marks_request(map_id, uid, region, cookie) {
            Ok(j) => { json = Some(j); used = region.to_string(); break; }
            Err(e) => { last_err = Some(e); }
        }
    }
    let json = json.ok_or_else(|| last_err.unwrap_or_else(|| anyhow::anyhow!("no region worked")))?;

    // Marks live in data.list[] with point_id (+ label_id, id).
    let arr = json
        .pointer("/data/list")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut point_ids = Vec::with_capacity(arr.len());
    for it in &arr {
        if let Some(id) = it.get("point_id").and_then(|v| v.as_u64()) {
            point_ids.push(id);
        }
    }
    let total = arr.len();
    tracing::info!(map_id, uid, %used, marks = total, parsed = point_ids.len(), "HoYoLab marks fetched");
    if point_ids.is_empty() && total > 0 {
        tracing::warn!("unparsed mark shape: {}", serde_json::to_string(
            &arr.first()).unwrap_or_default());
    }
    Ok(MarksResult { point_ids, total, region: used })
}
