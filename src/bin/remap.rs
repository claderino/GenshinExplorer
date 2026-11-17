//! 7.1 → 7.2 command-id remap analysis over local capture files.
//!
//! CmdIds rotate every major version, but message *field structure* rarely
//! does. This tool replays the exact 7.1 protobuf paths against every
//! captured command and groups hits by cmd id, so the same walk over the
//! 7.1 slice of a file must name the known 7.1 ids (sanity check) and the
//! 7.2 slice yields the new mapping.
//!
//! Usage: remap <file.jsonl> <ts-prefix>   e.g. remap fullcap.jsonl 2026-11-17
//! Lines are filtered by substring; both `cmd` and `command_id` keys work.

use std::collections::HashMap;
use std::io::{BufRead, Write};

// ---------------------------------------------------------------------------
// Minimal protobuf walker (same semantics as src/proto_walk.rs)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Val {
    Varint(u64),
    Fixed32(u32),
    Fixed64(u64),
    Bytes(Vec<u8>),
}

impl Val {
    fn varint(&self) -> Option<u64> {
        match self {
            Val::Varint(v) => Some(*v),
            _ => None,
        }
    }
    fn f32(&self) -> Option<f32> {
        match self {
            Val::Fixed32(b) => Some(f32::from_bits(*b)),
            _ => None,
        }
    }
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Val::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

fn read_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if shift >= 64 {
            return None;
        }
        v |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
        shift += 7;
    }
    None
}

type Fields = Vec<(u32, Val)>;

/// Tolerant parse (mirrors the repo's parse_partial): return the fields
/// that parsed cleanly, ignoring trailing garbage — strict parsing fails
/// on real captured payloads.
fn parse(buf: &[u8]) -> Option<Fields> {
    let mut fields = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let Some((key, n)) = read_varint(&buf[i..]) else { break };
        let fnum = (key >> 3) as u32;
        let wt = (key & 0x7) as u8;
        if fnum == 0 || fnum > 2_000_000 {
            break;
        }
        let v = match wt {
            0 => {
                let Some((v, m)) = read_varint(&buf[i + n..]) else { break };
                i += n + m;
                Val::Varint(v)
            }
            1 => {
                if i + n + 8 > buf.len() { break; }
                let v = u64::from_le_bytes(buf[i + n..i + n + 8].try_into().unwrap());
                i += n + 8;
                Val::Fixed64(v)
            }
            2 => {
                let Some((len, m)) = read_varint(&buf[i + n..]) else { break };
                let start = i + n + m;
                let len = len as usize;
                if start.checked_add(len).map(|e| e > buf.len()).unwrap_or(true) { break; }
                let b = buf[start..start + len].to_vec();
                i = start + len;
                Val::Bytes(b)
            }
            5 => {
                if i + n + 4 > buf.len() { break; }
                let v = u32::from_le_bytes(buf[i + n..i + n + 4].try_into().unwrap());
                i += n + 4;
                Val::Fixed32(v)
            }
            _ => break,
        };
        fields.push((fnum, v));
    }
    if fields.is_empty() { None } else { Some(fields) }
}

fn field<'a>(fs: &'a [((u32, Val))], num: u32) -> Vec<&'a Val> {
    fs.iter().filter(|(f, _)| *f == num).map(|(_, v)| v).collect()
}

fn hexbytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let hi = (b[i] as char).to_digit(16);
        let lo = (b[i + 1] as char).to_digit(16);
        if let (Some(h), Some(l)) = (hi, lo) {
            out.push((h * 16 + l) as u8);
        }
        i += 2;
    }
    out
}

// ---------------------------------------------------------------------------
// Signal detectors
// ---------------------------------------------------------------------------

/// Vector message: fields 1,2,3 all fixed32, plausible world coords.
fn as_vector(fs: &Fields) -> Option<(f32, f32, f32)> {
    let x = field(fs, 1).first().and_then(|v| v.f32())?;
    let y = field(fs, 2).first().and_then(|v| v.f32())?;
    let z = field(fs, 3).first().and_then(|v| v.f32())?;
    if x.is_finite() && y.is_finite() && z.is_finite()
        && x.abs() <= 9000.0 && y.abs() <= 4000.0 && z.abs() <= 9000.0
    {
        Some((x, y, z))
    } else {
        None
    }
}

/// 7.1 cmd-2246 batch movement: top f8 → f6 → f9 → f15 =
/// EntityMoveInfo { 1: entity_id, 2: MotionInfo { 1: Vector } }.
fn batch_path(buf: &[u8]) -> Option<(u64, (f32, f32, f32))> {
    let top = parse(buf)?;
    for f8 in field(&top, 8) {
        let Some(b8) = f8.bytes() else { continue };
        let Some(fs8) = parse(b8) else { continue };
        for f6 in field(&fs8, 6) {
            let Some(b6) = f6.bytes() else { continue };
            let Some(fs6) = parse(b6) else { continue };
            for f9 in field(&fs6, 9) {
                let Some(b9) = f9.bytes() else { continue };
                let Some(fs9) = parse(b9) else { continue };
                for f15 in field(&fs9, 15) {
                    let Some(b15) = f15.bytes() else { continue };
                    let Some(fs15) = parse(b15) else { continue };
                    let eid = field(&fs15, 1).first().and_then(|v| v.varint())?;
                    let mot = field(&fs15, 2).first().and_then(|v| v.bytes())?;
                    let fsm = parse(mot)?;
                    let vecb = field(&fsm, 1).first().and_then(|v| v.bytes())?;
                    let fsv = parse(vecb)?;
                    if let Some(pos) = as_vector(&fsv) {
                        return Some((eid, pos));
                    }
                }
            }
        }
    }
    None
}

/// 7.1 cmd-26016 direct position: top f1 → { 7: Vector, 11: varint }.
fn direct_path(buf: &[u8]) -> Option<((f32, f32, f32), u64)> {
    let top = parse(buf)?;
    for f1 in field(&top, 1) {
        let Some(b1) = f1.bytes() else { continue };
        let Some(fs1) = parse(b1) else { continue };
        let vecb = field(&fs1, 7).first().and_then(|v| v.bytes())?;
        let fsv = parse(vecb)?;
        let pos = as_vector(&fsv)?;
        let ts = field(&fs1, 11).first().and_then(|v| v.varint()).unwrap_or(0);
        return Some((pos, ts));
    }
    None
}

/// SceneAvatarInfo-ish: { 1: uid (9-digit), 2: avatar_id (1e7..13e6) }.
fn avatar_shapes(buf: &[u8], depth: u32, out: &mut Vec<(u32, u32)>) {
    if depth > 4 {
        return;
    }
    let Some(fs) = parse(buf) else { return };
    let uid = field(&fs, 1).first().and_then(|v| v.varint());
    let av = field(&fs, 2).first().and_then(|v| v.varint());
    if let (Some(u), Some(a)) = (uid, av) {
        if (100_000_000..=999_999_999).contains(&u)
            && (10_000_000..=13_000_000).contains(&a)
        {
            out.push((u as u32, a as u32));
            return;
        }
    }
    for (_, v) in &fs {
        if let Some(sub) = v.bytes() {
            avatar_shapes(sub, depth + 1, out);
        }
    }
}

/// Any-depth EntityMoveInfo shape: { 1: eid, 2: sub { 1: sub { vector } } }.
fn moveinfo_anywhere(buf: &[u8], depth: u32, out: &mut Vec<(u64, (f32, f32, f32))>) {
    if depth > 5 || !out.is_empty() {
        return;
    }
    let Some(fs) = parse(buf) else { return };
    let eid = field(&fs, 1).first().and_then(|v| v.varint());
    if let Some(eid) = eid {
        if let Some(mot) = field(&fs, 2).first().and_then(|v| v.bytes()) {
            if let Some(fsm) = parse(mot) {
                if let Some(vecb) = field(&fsm, 1).first().and_then(|v| v.bytes()) {
                    if let Some(fsv) = parse(vecb) {
                        if let Some(pos) = as_vector(&fsv) {
                            out.push((eid, pos));
                            return;
                        }
                    }
                }
            }
        }
    }
    for (_, v) in &fs {
        if let Some(sub) = v.bytes() {
            moveinfo_anywhere(sub, depth + 1, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CmdStats {
    total: u64,
    batch: u64,
    direct: u64,
    avatar: u64,
    moveany: u64,
    sample_batch: Option<(u64, (f32, f32, f32))>,
    sample_direct: Option<((f32, f32, f32), u64)>,
    sample_avatar: Option<(u32, u32)>,
    total_len: u64,
}

/// Recursive search for `{1: uid, 2: avatar_id}` with the exact uid;
/// records the field-number path taken.
fn find_avatar(buf: &[u8], uid: u64, path: &mut Vec<u32>, depth: u32) -> bool {
    if depth > 5 {
        return false;
    }
    let Some(fs) = parse(buf) else { return false };
    let f1 = fs.iter().find(|(f, _)| *f == 1).and_then(|(_, v)| v.varint());
    let f2 = fs.iter().find(|(f, _)| *f == 2).and_then(|(_, v)| v.varint());
    if let (Some(a), Some(b)) = (f1, f2) {
        if a == uid && (10_000_000..=13_000_000).contains(&b) {
            return true;
        }
    }
    for (n, v) in &fs {
        if let Some(sub) = v.bytes() {
            path.push(*n);
            if find_avatar(sub, uid, path, depth + 1) {
                return true;
            }
            path.pop();
        }
    }
    false
}

fn parse_partial_dbg(buf: &[u8]) -> Option<Fields> {
    let mut fields = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let Some((key, n)) = read_varint(&buf[i..]) else { break };
        let fnum = (key >> 3) as u32;
        let wt = (key & 0x7) as u8;
        if fnum == 0 { break; }
        let v = match wt {
            0 => { let Some((v, n)) = read_varint(&buf[i + n..]) else { break }; i += n; Val::Varint(v) }
            1 => { if i + n + 8 > buf.len() { break }; let v = u64::from_le_bytes(buf[i + n..i + n + 8].try_into().unwrap()); i += 8; Val::Fixed64(v) }
            2 => { let Some((len, m)) = read_varint(&buf[i + n..]) else { break }; i += n + m; let len = len as usize; if i + len > buf.len() { break }; let b = buf[i..i + len].to_vec(); i += len; Val::Bytes(b) }
            5 => { if i + n + 4 > buf.len() { break }; let v = u32::from_le_bytes(buf[i + n..i + n + 4].try_into().unwrap()); i += 4; Val::Fixed32(v) }
            _ => break,
        };
        fields.push((fnum, v));
    }
    Some(fields)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: remap <file> <ts-prefix> | remap --check <hex> | remap --uid <file> <prefix> <uid>");
        std::process::exit(2);
    };
    if path == "--uid" {
        // Find payloads containing a SceneAvatarInfo-ish {1: uid, 2:
        // avatar_id} for the given uid at any depth; print the enclosing
        // message's field layout so the carrier structure is readable.
        let file = args.next().expect("file");
        let prefix = args.next().unwrap_or_default();
        let uid: u64 = args.next().expect("uid").parse().expect("uid");
        let reader =
            std::io::BufReader::new(std::fs::File::open(&file).expect("open"));
        let mut found = 0u32;
        'outer: for line in reader.lines() {
            let Ok(line) = line else { break };
            if !line.contains(&prefix) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(cmd) = v
                .get("cmd")
                .or_else(|| v.get("command_id"))
                .and_then(|x| x.as_u64())
            else { continue };
            let Some(hex) = v.get("hex").and_then(|x| x.as_str()) else { continue };
            let buf = hexbytes(hex);
            // Path of field numbers from root to the avatar submessage.
            let mut path: Vec<u32> = Vec::new();
            if find_avatar(&buf, uid, &mut path, 0) {
                println!("cmd {cmd}: path={path:?} len={}", buf.len());
                if let Some(top) = parse(&buf) {
                    println!("   top: {:?}", top.iter().map(|(n, v)| (*n, match v {
                        Val::Bytes(b) => format!("b:{}", b.len()),
                        Val::Varint(x) => format!("v:{x}"),
                        _ => "fx".into(),
                    })).collect::<Vec<_>>());
                }
                found += 1;
                if found >= 12 { break 'outer; }
            }
        }
        if found == 0 { println!("no payloads carry uid {uid}"); }
        return;
    }
    if path == "--check" {
        let hex = args.next().unwrap_or_default();
        let buf = hexbytes(&hex);
        println!("batch_path: {:?}", batch_path(&buf));
        println!("direct_path: {:?}", direct_path(&buf));
        let mut avs = Vec::new();
        avatar_shapes(&buf, 0, &mut avs);
        println!("avatar_shapes: {avs:?}");
        let mut mv = Vec::new();
        moveinfo_anywhere(&buf, 0, &mut mv);
        println!("moveinfo_anywhere: {mv:?}");
        println!("top-level parse: {:?}", parse(&buf).map(|f| f
            .iter().map(|(n, v)| (*n, match v {
                Val::Varint(x) => format!("v:{x}"),
                Val::Fixed32(x) => format!("f32:{}", f32::from_bits(*x)),
                Val::Fixed64(_) => "f64".into(),
                Val::Bytes(b) => format!("b:{}", b.len()),
            })).collect::<Vec<_>>()));
        // Level-by-level batch walk: f8 → f6 → f9 → f15
        if let Some(top) = parse(&buf) {
            for (n, v) in &top {
                let Some(b8) = v.bytes() else { continue };
                let Some(fs8) = parse(b8) else {
                    println!("L8(f{n}): STRICT PARSE FAIL ({} bytes)", b8.len());
                    if let Some(f) = parse_partial_dbg(b8) {
                        println!("  partial: {f:?}");
                    }
                    continue;
                };
                println!("L8(f{n}): {:?}", fs8.iter().map(|(a, b)| (a, match b { Val::Bytes(x) => format!("b:{}", x.len()), _ => "v".into() })).collect::<Vec<_>>());
                for (n6, v6) in &fs8 {
                    let Some(b6) = v6.bytes() else { continue };
                    let Some(fs6) = parse(b6) else { println!("  L6(f{n6}): STRICT FAIL ({}b)", b6.len()); continue };
                    println!("  L6(f{n6}): {:?}", fs6.iter().map(|(a, b)| (a, match b { Val::Bytes(x) => format!("b:{}", x.len()), _ => "v".into() })).collect::<Vec<_>>());
                    for (n9, v9) in &fs6 {
                        let Some(b9) = v9.bytes() else { continue };
                        let fs9 = match parse(b9) {
                            Some(f) => f,
                            None => { println!("    L9(f{n9}): STRICT FAIL ({}b)", b9.len()); continue; }
                        };
                        println!("    L9(f{n9}): {:?}", fs9.iter().map(|(a, b)| (a, match b { Val::Bytes(x) => format!("b:{}", x.len()), _ => "v".into() })).collect::<Vec<_>>());
                    }
                }
            }
        }
        return;
    }
    let prefix = args.next().unwrap_or_default();

    let file = std::fs::File::open(&path).expect("open");
    let reader = std::io::BufReader::new(file);
    let mut stats: HashMap<u64, CmdStats> = HashMap::new();
    let mut lines = 0u64;

    for line in reader.lines() {
        let Ok(line) = line else { break };
        if !line.contains(&prefix) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let cmd = v
            .get("cmd")
            .or_else(|| v.get("command_id"))
            .and_then(|x| x.as_u64());
        let Some(cmd) = cmd else { continue };
        let hex = v.get("hex").and_then(|x| x.as_str()).unwrap_or("");
        let n = v.get("n").and_then(|x| x.as_u64()).unwrap_or(1);
        let len = v.get("len").and_then(|x| x.as_u64()).unwrap_or(hex.len() as u64 / 2);
        let buf = hexbytes(hex);
        lines += 1;

        // NB: "n" is a per-run sequence counter, not a count.
        let e = stats.entry(cmd).or_default();
        e.total += 1;
        e.total_len += len;
        if buf.is_empty() {
            continue;
        }
        if let Some((eid, pos)) = batch_path(&buf) {
            e.batch += 1;
            if e.sample_batch.is_none() {
                e.sample_batch = Some((eid, pos));
            }
        }
        if let Some(s) = direct_path(&buf) {
            e.direct += 1;
            if e.sample_direct.is_none() {
                e.sample_direct = Some(s);
            }
        }
        let mut avs = Vec::new();
        avatar_shapes(&buf, 0, &mut avs);
        if !avs.is_empty() {
            e.avatar += 1;
            if e.sample_avatar.is_none() {
                e.sample_avatar = avs.first().copied();
            }
        }
        let mut mv = Vec::new();
        moveinfo_anywhere(&buf, 0, &mut mv);
        if !mv.is_empty() {
            e.moveany += 1;
        }
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "== {path} [{prefix}] — {lines} lines ==");
    let mut show = |title: &str, sel: &dyn Fn(&CmdStats) -> u64| {
        let mut rows: Vec<_> = stats
            .iter()
            .filter(|(_, s)| sel(s) > 0)
            .collect();
        rows.sort_by_key(|(_, s)| std::cmp::Reverse(sel(s)));
        let _ = writeln!(out, "\n-- {title} --");
        for (cmd, s) in rows.iter().take(8) {
            let _ = write!(
                out,
                "  cmd {cmd}: hits={} of {}  ",
                sel(s),
                s.total
            );
            if let Some((eid, (x, y, z))) = &s.sample_batch {
                let _ = write!(out, "batch[eid={eid} pos=({x:.1},{y:.1},{z:.1})] ");
            }
            if let Some(((x, y, z), ts)) = &s.sample_direct {
                let _ = write!(out, "direct[pos=({x:.1},{y:.1},{z:.1}) ts={ts}] ");
            }
            if let Some((uid, av)) = &s.sample_avatar {
                let _ = write!(out, "avatar[uid={uid} av={av}] ");
            }
            let _ = writeln!(out);
        }
    };
    show("7.1 batch path (was 2246)", &|s: &CmdStats| s.batch);
    show("7.1 direct path (was 26016)", &|s: &CmdStats| s.direct);
    show("avatar shape (was 27685 family)", &|s: &CmdStats| s.avatar);
    show("EntityMoveInfo anywhere", &|s: &CmdStats| s.moveany);

    let _ = writeln!(out, "\n-- top by volume --");
    let mut rows: Vec<_> = stats.iter().collect();
    rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.total));
    for (cmd, s) in rows.iter().take(10) {
        let _ = writeln!(
            out,
            "  cmd {cmd}: n={} avg_len={}",
            s.total,
            if s.total > 0 { s.total_len / s.total } else { 0 }
        );
    }
}
