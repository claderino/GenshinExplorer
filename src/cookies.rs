//! Auto-detection of the HoYoLab session cookie from local browsers.
//!
//!  * Firefox: `cookies.sqlite` stores values in PLAINTEXT — reliable.
//!  * Edge / Chrome: `Network\Cookies` values are DPAPI + AES-256-GCM
//!    encrypted (v10/v11 prefix); best-effort — newer Chrome app-binds
//!    the key beyond our reach, in which case manual paste remains.
//!
//! Only `.hoyolab.com` cookies are read; nothing leaves the machine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The cookie names that make a usable session.
const REQUIRED: &[&str] = &["cookie_token_v2", "account_id_v2", "ltoken_v2", "ltuid_v2"];
/// Everything worth sending (auth + device identity).
const WANTED: &[&str] = &[
    "cookie_token_v2", "account_id_v2", "account_mid_v2",
    "ltoken_v2", "ltuid_v2", "ltmid_v2",
    "_HYVUUID", "DEVICEFP",
];

#[derive(Clone)]
pub struct FoundCookie {
    pub cookie: String,
    pub source: String,
    /// Game uid when the entry is bound to a specific character/region.
    pub uid: Option<u32>,
}

/// Every complete session found across browsers — Firefox profile ×
/// container, plus each Chromium profile.
pub fn auto_detect_all() -> Vec<FoundCookie> {
    let mut out = Vec::new();
    out.extend(firefox_all());
    for (name, finder) in [
        ("edge", chromium_roots as fn(&str) -> Vec<PathBuf>),
        ("chrome", chromium_roots as fn(&str) -> Vec<PathBuf>),
    ] {
        out.extend(chromium_all(name, finder));
    }
    out
}

/// Try Firefox first, then Chromium-based browsers.
pub fn auto_detect() -> Result<FoundCookie> {
    let mut errors: Vec<String> = Vec::new();
    match firefox() {
        Ok(c) => return Ok(c),
        Err(e) => errors.push(format!("firefox: {e}")),
    }
    for (name, finder) in [
        ("edge", chromium_roots as fn(&str) -> Vec<PathBuf>),
        ("chrome", chromium_roots as fn(&str) -> Vec<PathBuf>),
    ] {
        match chromium(name, finder) {
            Ok(c) => return Ok(c),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    bail!("no browser cookie found ({})", errors.join("; "));
}

// ---------------------------------------------------------------------------
// Firefox (plaintext sqlite, per-container cookie jars)
// ---------------------------------------------------------------------------

/// Container names from the profile's containers.json. Named containers
/// carry `name`; Firefox's built-ins carry only `l10nId` localization keys
/// (resolved to their English defaults).
fn container_names(profile_dir: &Path) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(profile_dir.join("containers.json"))
    else {
        return map;
    };
    // The file may start with comment lines; strip them and parse as JSON.
    let json_text: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_text) else {
        return map;
    };
    let Some(list) = json.get("identities").and_then(|v| v.as_array()) else {
        return map;
    };
    for id in list {
        let Some(uc) = id.get("userContextId").and_then(|v| v.as_u64()) else {
            continue;
        };
        if uc > 1_000_000 {
            continue; // internal bookkeeping ids
        }
        let name = id
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|n| !n.starts_with("userContextIdInternal"))
            .map(|n| n.to_string())
            .or_else(|| {
                id.get("l10nId")
                    .and_then(|v| v.as_str())
                    .and_then(l10n_container_name)
            });
        if let Some(name) = name {
            if !name.is_empty() {
                map.insert(uc as u32, name);
            }
        }
    }
    map
}

fn l10n_container_name(l10n: &str) -> Option<String> {
    Some(
        match l10n {
            "user-context-personal" => "Personal",
            "user-context-work" => "Work",
            "user-context-banking" => "Banking",
            "user-context-shopping" => "Shopping",
            _ => return None,
        }
        .to_string(),
    )
}

/// All Firefox sessions: every profile × every container that holds a
/// complete auth set.
fn firefox_all() -> Vec<FoundCookie> {
    let profiles = PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default())
        .join("Mozilla/Firefox/Profiles");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&profiles) else { return out; };
    for e in rd.flatten() {
        let db = e.path().join("cookies.sqlite");
        if !db.exists() { continue; }
        let profile_name = e.file_name().to_string_lossy().to_string();
        let containers = container_names(&e.path());
        let tmp = std::env::temp_dir().join("gx-ff-cookies.sqlite");
        if std::fs::copy(&db, &tmp).is_err() { continue; }
        let Ok(conn) = open_ro(&tmp) else { continue; };
        let Ok(mut stmt) = conn.prepare(
            "SELECT name, value, originAttributes FROM moz_cookies \
             WHERE host LIKE '%hoyolab.com'",
        ) else { continue; };
        let Ok(rows) = stmt.query_map([], |r| Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2).unwrap_or_default(),
        ))) else { continue; };
        // Group by cookie jar (originAttributes).
        let mut jars: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for row in rows.flatten() {
            jars.entry(row.2).or_default().push((row.0, row.1));
        }
        for (attrs, pairs) in jars {
            if let Some(cookie) = assemble(pairs) {
                let container = attrs
                    .split("userContextId=")
                    .nth(1)
                    .and_then(|s| s.split(&[',', '^', ' '][..]).next())
                    .and_then(|s| s.parse::<u32>().ok());
                let source = match container {
                    None | Some(0) => format!("Firefox ({profile_name})"),
                    Some(id) => match containers.get(&id) {
                        Some(name) => format!("Firefox {name} ({profile_name})"),
                        None => format!("Firefox container {id} ({profile_name})"),
                    },
                };
                out.push(FoundCookie { cookie, source, uid: None });
            }
        }
    }
    out
}

fn firefox() -> Result<FoundCookie> {
    firefox_all().into_iter().next()
        .context("no firefox session (logged into hoyolab?)")
}

// ---------------------------------------------------------------------------
// Chromium: Edge / Chrome (DPAPI + AES-GCM)
// ---------------------------------------------------------------------------

fn chromium_roots(browser: &str) -> Vec<PathBuf> {
    let base = PathBuf::from(
        std::env::var_os("LOCALAPPDATA").unwrap_or_default(),
    );
    let user_data = match browser {
        "edge" => base.join("Microsoft/Edge/User Data"),
        _ => base.join("Google/Chrome/User Data"),
    };
    let mut roots = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&user_data) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "Default" || name.starts_with("Profile ") {
                roots.push(e.path());
            }
        }
    }
    roots
}

fn chromium_all(browser: &str, roots: fn(&str) -> Vec<PathBuf>) -> Vec<FoundCookie> {
    let mut out = Vec::new();
    let Some(user_data) = roots(browser).first()
        .and_then(|p| p.parent().map(|p| p.to_path_buf())) else { return out; };
    let Ok(key) = chromium_key(&user_data) else { return out; };
    for profile in roots(browser) {
        let db = profile.join("Network/Cookies");
        if !db.exists() { continue; }
        let tmp = std::env::temp_dir().join("gx-ch-cookies.sqlite");
        if std::fs::copy(&db, &tmp).is_err() { continue; }
        let Ok(rows) = read_sqlite_encrypted(&tmp) else { continue; };
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (name, enc) in rows {
            if let Some(v) = decrypt_v10(&key, &enc) {
                pairs.push((name, v));
            }
        }
        if let Some(cookie) = assemble(pairs) {
            out.push(FoundCookie {
                cookie,
                source: format!("{browser} ({})",
                    profile.file_name().unwrap_or_default().to_string_lossy()),
                uid: None,
            });
        }
    }
    out
}

fn chromium(
    browser: &str,
    roots: fn(&str) -> Vec<PathBuf>,
) -> Result<FoundCookie> {
    chromium_all(browser, roots).into_iter().next()
        .context("no usable session (logged in? key app-bound?)")
}

/// `Local State` → `os_crypt.encrypted_key` → DPAPI → 32-byte AES key.
fn chromium_key(user_data: &Path) -> Result<[u8; 32]> {
    let ls: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(user_data.join("Local State"))
            .context("Local State missing")?,
    )?;
    let b64 = ls.pointer("/os_crypt/encrypted_key")
        .and_then(|v| v.as_str()).context("encrypted_key missing")?;
    let mut blob = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        b64.trim_end_matches('"'),
    )?;
    if blob.starts_with(b"DPAPI") { blob.drain(0..5); }
    let plain = dpapi_unprotect(&blob).context("DPAPI unprotect failed")?;
    let key: [u8; 32] = plain.as_slice().try_into()
        .map_err(|_| anyhow::anyhow!("bad key length"))?;
    Ok(key)
}

/// Chrome ≥ v80: `v10`/`v11` prefix + 12-byte nonce + AES-256-GCM
/// ciphertext (tag appended).
fn decrypt_v10(key: &[u8; 32], enc: &[u8]) -> Option<String> {
    use aes_gcm::{Aes256Gcm, KeyInit, aead::Aead, Nonce};
    if enc.len() < 3 + 12 + 16 || !enc.starts_with(b"v1") { return None; }
    let nonce = Nonce::from_slice(&enc[3..15]);
    let cipher = Aes256Gcm::new(key.into());
    let plain = cipher.decrypt(nonce, &enc[15..]).ok()?;
    Some(String::from_utf8_lossy(&plain).into_owned())
}

#[cfg(windows)]
fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();
        if CryptUnprotectData(
            &mut in_blob,
            None,
            None,
            None,
            None,
            0,
            &mut out_blob,
        )
        .is_err()
        {
            bail!("CryptUnprotectData failed");
        }
        let out = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize)
            .to_vec();
        // LocalFree(out_blob.pbData) — leaked once per call; acceptable.
        Ok(out)
    }
}

#[cfg(windows)]
pub fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let mut in_blob = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();
        if CryptProtectData(
            &mut in_blob,
            None,
            None,
            None,
            None,
            0,
            &mut out_blob,
        )
        .is_err()
        {
            bail!("CryptProtectData failed");
        }
        let out = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize)
            .to_vec();
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Named cookie profiles (DPAPI-encrypted at rest)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct CookieProfile {
    pub name: String,
    /// Optional game UID this account syncs (defaults to the active game uid).
    pub uid: Option<u32>,
    pub cookie: String,
}

fn profiles_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|v|
        PathBuf::from(v).join("GenshinExplorer").join("hoyolab_profiles.json"))
}

/// Load profiles; entries that fail to decrypt are skipped.
pub fn load_profiles() -> Vec<CookieProfile> {
    let Ok(text) = std::fs::read_to_string(
        profiles_path().unwrap_or_default(),
    ) else {
        return Vec::new();
    };
    let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else {
        return Vec::new();
    };
    list.iter().filter_map(|p| {
        let name = p.get("name")?.as_str()?.to_string();
        let uid = p.get("uid").and_then(|v| v.as_u64()).map(|u| u as u32);
        let b64 = p.get("enc")?.as_str()?;
        let enc = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD, b64,
        ).ok()?;
        let cookie_bytes = dpapi_unprotect(&enc).ok()?;
        let cookie = String::from_utf8(cookie_bytes).ok()?;
        if cookie.contains("cookie_token_v2") {
            Some(CookieProfile { name, uid, cookie })
        } else {
            None
        }
    }).collect()
}

/// Persist profiles with each cookie DPAPI-encrypted.
pub fn save_profiles(profiles: &[CookieProfile]) -> Result<()> {
    let path = profiles_path().context("no LOCALAPPDATA")?;
    let list: Vec<serde_json::Value> = profiles.iter().filter_map(|p| {
        let enc = dpapi_protect(p.cookie.as_bytes()).ok()?;
        Some(serde_json::json!({
            "name": p.name,
            "uid": p.uid,
            "enc": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD, &enc),
        }))
    }).collect();
    std::fs::write(&path, serde_json::to_string_pretty(&list)?)?;
    Ok(())
}

#[cfg(not(windows))]
fn dpapi_unprotect(_blob: &[u8]) -> Result<Vec<u8>> {
    bail!("windows only")
}

// ---------------------------------------------------------------------------
// SQLite helpers
// ---------------------------------------------------------------------------

fn open_ro(path: &Path) -> Result<rusqlite::Connection> {
    Ok(rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

/// Chromium-style cookies table with encrypted_value.
fn read_sqlite_encrypted(db: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let conn = open_ro(db)?;
    let mut stmt = conn.prepare(
        "SELECT name, encrypted_value FROM cookies \
         WHERE host_key LIKE '%hoyolab.com'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Vec<u8>>(1)?,
        ))
    })?;
    Ok(rows.flatten().collect())
}

/// Assemble must reject incomplete sessions.
#[test]
fn assemble_requires_auth_keys() {
    let partial = vec![("cookie_token_v2".to_string(), "x".to_string())];
    assert!(assemble(partial).is_none());
}

/// Real-machine check: auto-detect finds a usable cookie (source printed,
/// cookie value never logged).
#[test]
fn auto_detect_finds_real_cookie() {
    match auto_detect() {
        Ok(found) => {
            assert!(found.cookie.contains("cookie_token_v2"));
            assert!(found.cookie.contains("account_id_v2"));
            eprintln!("auto-detect source: {}", found.source);
        }
        Err(e) => {
            eprintln!("auto-detect unavailable here: {e:#}");
            // Not a failure — CI machines have no browser session.
        }
    }
}

/// Keep the WANTED cookie names, require the auth essentials.
fn assemble(pairs: Vec<(String, String)>) -> Option<String> {
    let map: std::collections::HashMap<String, String> = pairs
        .into_iter()
        .filter(|(n, v)| WANTED.contains(&n.as_str()) && !v.is_empty())
        .collect();
    if !REQUIRED.iter().all(|r| map.contains_key(*r)) {
        return None;
    }
    Some(
        WANTED.iter()
            .filter_map(|n| map.get(*n).map(|v| format!("{n}={v}")))
            .collect::<Vec<_>>()
            .join("; "),
    )
}
