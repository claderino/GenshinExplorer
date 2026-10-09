//! Value-pattern detection of Mora events in decoded game commands.
//!
//! HoYo scrambles command IDs every version and has reshuffled field numbers
//! between major versions, so nothing here relies on a fixed command ID.
//! Instead we recognize packets by their content:
//!
//! - Prop change:   a small message containing the Mora property id (10016),
//!                  two balance-ish numbers (old/current) and a small reason code.
//!                  (PlayerPropChangeReasonNotify — old/new may be floats or ints
//!                  depending on version)
//! - Balance sync:  map-entry / PropValue shaped submessages keyed by 10016.
//!                  (PlayerPropNotify, PlayerDataNotify at login)
//! - Item add:      ItemParam-shaped submessages `{item_id: 202, count}` plus an
//!                  ActionReason code. This is how Mora from monsters/chests/quests
//!                  arrives, and it carries the richest "what action was this" info.

use std::collections::HashMap;

use auto_artifactarium::GameCommand;

use crate::proto_walk::{Fields, Value, parse};
use crate::reasons::{action_reason_name, prop_change_reason_name};

/// Mora player property id (`PROP_PLAYER_SCOIN`).
pub const PROP_MORA: u64 = 10016;
/// Mora as a virtual item id (`ITEM_VIRTUAL_SCOIN`).
pub const ITEM_MORA: u64 = 202;

/// Sanity ceiling for a Mora balance. Nobody legitimately holds more.
const MAX_BALANCE: f64 = 2_000_000_000.0;
/// Largest single-event delta we accept (filters misparses).
const MAX_DELTA: f64 = 100_000_000.0;

#[derive(Debug, Clone, PartialEq)]
pub enum MoraEvent {
    /// Server-authoritative balance transition with a reason code
    /// (`PlayerPropChangeReasonNotify` shaped).
    PropChange {
        old: f64,
        new: f64,
        prop_reason: Option<u32>,
        command_id: u16,
    },
    /// Mora obtained as an item reward with an action reason
    /// (`ItemAddNotify` shaped, item id 202).
    ItemAdd {
        count: u64,
        action_reason: Option<u32>,
        command_id: u16,
    },
    /// Balance snapshot observed in a prop map
    /// (`PlayerPropNotify` / login data shaped).
    Balance {
        balance: f64,
        command_id: u16,
    },
}

impl MoraEvent {
    #[allow(dead_code)] // useful for future diagnostics/UI work
    pub fn command_id(&self) -> u16 {
        match self {
            MoraEvent::PropChange { command_id, .. }
            | MoraEvent::ItemAdd { command_id, .. }
            | MoraEvent::Balance { command_id, .. } => *command_id,
        }
    }

    /// Signed delta where meaningful; `None` for plain balance snapshots.
    #[allow(dead_code)] // useful for future diagnostics/UI work
    pub fn delta(&self) -> Option<i64> {
        match self {
            MoraEvent::PropChange { old, new, .. } => Some((new - old).round() as i64),
            MoraEvent::ItemAdd { count, .. } => Some(*count as i64),
            MoraEvent::Balance { .. } => None,
        }
    }

    /// Human readable reason, if any.
    #[allow(dead_code)] // useful for future diagnostics/UI work
    pub fn reason_label(&self) -> String {
        match self {
            MoraEvent::PropChange { prop_reason: Some(r), .. } => {
                format!("PropChange:{}", prop_change_reason_name(*r).unwrap_or("?"))
            }
            MoraEvent::ItemAdd { action_reason: Some(r), .. } => crate::reasons::action_reason_label(*r),
            _ => "unknown".to_string(),
        }
    }
}

/// Run all detectors over a decoded command. `proto_data` is the body with
/// the PacketHead already separated (vendored upstream fix), so it is
/// analyzed directly; a one-level-deep submessage scan catches layouts where
/// payloads are wrapped an extra level.
pub fn detect_mora_events(cmd: &GameCommand) -> Vec<MoraEvent> {
    if let Some(fields) = parse(&cmd.proto_data[..]) {
        let mut events = detect_prop_change(&fields, cmd.command_id);
        events.extend(detect_balance(&fields, cmd.command_id));
        events.extend(detect_item_adds(&fields, cmd.command_id));
        if !events.is_empty() {
            return events;
        }
    }

    // Last resort: some versions wrap payloads one level deep.
    if let Some(fields) = parse(&cmd.proto_data[..]) {
        let mut events = Vec::new();
        for (_, value) in &fields {
            if let Some(bytes) = value.as_bytes() {
                if let Some(sub) = parse(bytes) {
                    events.extend(detect_prop_change(&sub, cmd.command_id));
                    events.extend(detect_balance(&sub, cmd.command_id));
                }
            }
        }
        if !events.is_empty() {
            return events;
        }
    }

    Vec::new()
}

/// `PlayerPropChangeReasonNotify` shape: a lean message with a varint equal to
/// the Mora prop id, two numeric balances and (usually) a small reason code.
fn detect_prop_change(fields: &Fields, command_id: u16) -> Vec<MoraEvent> {
    // Locate the field that carries the Mora property id.
    let prop_field = fields
        .iter()
        .find(|(_, v)| v.as_varint() == Some(PROP_MORA))
        .map(|(f, _)| *f);
    let Some(prop_field) = prop_field else {
        return Vec::new();
    };

    // Locate old/cur balances. Every observed version encodes them as
    // float32 (fixed32), so prefer float pairs; fall back to integer
    // varints for hypothetical future layouts. In both known layouts "old"
    // carries the lower field number and "cur" the higher one
    // (3.x: 2/3, 2.x: 10/11).
    let mut floats: Vec<(u32, f64)> = fields
        .iter()
        .filter_map(|(f, v)| {
            let x = v.as_f32()? as f64;
            (x.is_finite() && (0.0..=MAX_BALANCE).contains(&x)).then_some((*f, x))
        })
        .collect();

    let (old, new) = if floats.len() >= 2 {
        floats.sort_by_key(|(f, _)| *f);
        (floats.first().unwrap().1, floats.last().unwrap().1)
    } else {
        let mut vars: Vec<(u32, f64)> = fields
            .iter()
            .filter(|(f, _)| *f != prop_field)
            .filter_map(|(f, v)| match v {
                Value::Varint(x) if *x <= MAX_BALANCE as u64 => Some((*f, *x as f64)),
                _ => None,
            })
            .collect();
        if vars.len() >= 3 {
            // Several integer candidates: drop small values (reason codes,
            // flags) unless that would starve us of a pair.
            let retained: Vec<_> = vars.iter().filter(|(_, x)| *x >= 1000.0).collect();
            if retained.len() >= 2 {
                vars.retain(|(_, x)| *x >= 1000.0);
            }
        }
        if vars.len() < 2 {
            return Vec::new();
        }
        vars.sort_by_key(|(f, _)| *f);
        (vars.first().unwrap().1, vars.last().unwrap().1)
    };

    // Reject no-op transitions and absurd jumps.
    if (new - old).abs() < 0.5 || (new - old).abs() > MAX_DELTA {
        return Vec::new();
    }

    // Reason: a small varint distinct from the prop field, plausible for the
    // PropChangeReason enum (values 0..=13).
    let reason = fields
        .iter()
        .filter(|(f, _)| *f != prop_field)
        .filter_map(|(f, v)| v.as_varint().map(|x| (*f, x)))
        .find(|(_, x)| *x <= 100)
        .map(|(_, x)| x as u32)
        .filter(|r| prop_change_reason_name(*r).is_some());

    vec![MoraEvent::PropChange {
        old,
        new,
        prop_reason: reason,
        command_id,
    }]
}

/// `PlayerPropNotify` / login `prop_map` shape: map entries
/// `{1: key=10016, 2: PropValue{1: type=10016, 2: value}}` or a bare
/// `PropValue` list.
fn detect_balance(fields: &Fields, command_id: u16) -> Vec<MoraEvent> {
    let mut events = Vec::new();
    for (_, value) in fields {
        let Some(bytes) = value.as_bytes() else { continue };
        let Some(sub) = parse(bytes) else { continue };

        if let Some(balance) = mora_prop_value(&sub) {
            events.push(MoraEvent::Balance { balance, command_id });
            continue;
        }

        // Map entry: field 1 is the key (10016), field 2 the PropValue.
        let is_mora_entry = sub
            .iter()
            .any(|(f, v)| *f == 1 && v.as_varint() == Some(PROP_MORA));
        if is_mora_entry {
            for (_, v2) in &sub {
                if let Some(inner) = v2.as_bytes() {
                    if let Some(sub2) = parse(inner) {
                        if let Some(balance) = mora_prop_value(&sub2) {
                            events.push(MoraEvent::Balance { balance, command_id });
                        }
                    }
                }
            }
        }
    }
    events
}

/// `PropValue { 1: type, 2: ival, 4: val }` where type == Mora.
fn mora_prop_value(sub: &Fields) -> Option<f64> {
    let has_mora_type = sub
        .iter()
        .any(|(f, v)| *f == 1 && v.as_varint() == Some(PROP_MORA));
    if !has_mora_type {
        return None;
    }
    sub.iter()
        .filter(|(f, _)| *f == 2 || *f == 4)
        .find_map(|(_, v)| match v {
            Value::Varint(x) if *x <= MAX_BALANCE as u64 => Some(*x as f64),
            Value::Fixed32(_) => {
                let x = v.as_f32().unwrap() as f64;
                (x.is_finite() && (0.0..=MAX_BALANCE).contains(&x)).then_some(x)
            }
            _ => None,
        })
}

/// `ItemAddNotify` shape: repeated `{1: item_id=202, 2: count}` (ItemParam)
/// or `{1: item_id=202, 5: {1: count}}` (full Item), plus a top-level
/// ActionReason varint.
fn detect_item_adds(fields: &Fields, command_id: u16) -> Vec<MoraEvent> {
    let mut counts: Vec<u64> = Vec::new();
    // Non-Mora item content present (for Mora-less chest detection).
    let mut any_item = false;

    for (_, value) in fields {
        let Some(bytes) = value.as_bytes() else { continue };
        let Some(sub) = parse(bytes) else { continue };

        let id_is_mora = sub
            .iter()
            .any(|(f, v)| *f == 1 && v.as_varint() == Some(ITEM_MORA));

        // Any plausible item id (materials, weapons, quest items, …) marks
        // item content.
        if sub.iter().any(|(f, v)| {
            *f == 1
                && v.as_varint()
                    .map_or(false, |i| (100..=99_999_999).contains(&i))
        }) {
            any_item = true;
        }

        if !id_is_mora {
            continue;
        }

        // ItemParam: count directly on field 2.
        if let Some(count) = sub
            .iter()
            .filter(|(f, _)| *f == 2)
            .find_map(|(_, v)| v.as_varint())
            .filter(|c| (1..=10_000_000).contains(c))
        {
            counts.push(count);
            continue;
        }

        // Full Item: material detail on field 5 with count on its field 1.
        for (_, v2) in &sub {
            if let Some(inner) = v2.as_bytes() {
                if let Some(sub2) = parse(inner) {
                    if let Some(count) = sub2
                        .iter()
                        .filter(|(f, _)| *f == 1)
                        .find_map(|(_, v)| v.as_varint())
                        .filter(|c| (1..=10_000_000).contains(c))
                    {
                        counts.push(count);
                    }
                }
            }
        }
    }

    if counts.is_empty() && !any_item {
        return Vec::new();
    }

    // ActionReason: a top-level varint that maps into the known enum.
    let reason = fields
        .iter()
        .filter_map(|(_, v)| v.as_varint())
        .filter(|x| *x <= 2000 && action_reason_name(*x as u32).is_some())
        .last()
        .map(|x| x as u32);

    let mut out: Vec<MoraEvent> = counts
        .into_iter()
        .map(|count| MoraEvent::ItemAdd {
            count,
            action_reason: reason,
            command_id,
        })
        .collect();

    // Mora-less chest: item content under a chest-family reason still means
    // a chest opened (some chests give no Mora at all).
    if out.is_empty() && any_item && matches!(reason, Some(39 | 52 | 55)) {
        out.push(MoraEvent::ItemAdd {
            count: 0,
            action_reason: reason,
            command_id,
        });
    }
    out
}

/// Candidate self-UIDs from SceneAvatarInfo-shaped submessages:
/// `{ 1: uid (9 digits), 2: avatar_id (10xxxxxx range) }`. At login every
/// avatar entity carries the local player's UID, so frequent values here
/// identify the account's in-game UID (which 7.x removed from wire headers).
pub fn detect_uid_candidates(cmd: &GameCommand, votes: &mut HashMap<u32, u32>) {
    collect_uid_votes(&cmd.proto_data, votes, 0);
}

fn collect_uid_votes(buf: &[u8], votes: &mut HashMap<u32, u32>, depth: u32) {
    if depth > 3 {
        return;
    }
    let Some(fields) = parse(buf) else { return };
    for (_, value) in &fields {
        if let Some(sub) = value.as_bytes() {
            if let Some(sub_fields) = parse(sub) {
                if let Some(uid) = avatar_owner_shape(&sub_fields) {
                    *votes.entry(uid).or_default() += 1;
                } else {
                    collect_uid_votes(sub, votes, depth + 1);
                }
            }
        }
    }
}

/// Matches a submessage shaped like `{ 1: uid, 2: avatar_id }` — i.e. a
/// SceneAvatarInfo. Returns the uid.
fn avatar_owner_shape(sub: &Fields) -> Option<u32> {
    let mut uid: Option<u32> = None;
    let mut has_avatar_id = false;
    for (field, value) in sub {
        match (*field, value.as_varint()) {
            (1, Some(v)) if (100_000_000..=999_999_999).contains(&v) => uid = Some(v as u32),
            (2, Some(v)) if (10_000_000..=13_000_000).contains(&v) => has_avatar_id = true,
            _ => {}
        }
    }
    uid.filter(|_| has_avatar_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_command(proto: &[u8]) -> GameCommand {
        GameCommand {
            command_id: 1234,
            header_len: 0,
            data_len: proto.len() as u32,
            proto_header: Vec::new(),
            proto_data: proto.to_vec(),
        }
    }

    fn varint_field(field: u32, value: u64) -> Vec<u8> {
        let mut out = vec![(field << 3) as u8];
        out.extend_from_slice(&encode_varint(value));
        out
    }

    fn float_field(field: u32, value: f32) -> Vec<u8> {
        let mut out = vec![((field << 3) | 5) as u8];
        out.extend_from_slice(&value.to_bits().to_le_bytes());
        out
    }

    fn bytes_field(field: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(field << 3) as u8 | 2];
        out.extend_from_slice(&encode_varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out
    }

    #[test]
    fn detects_prop_change_3x_layout() {
        // prop_type=1 (10016), old=2 (float 1234000.0), cur=3 (float 1234500.0), reason=4 (11)
        let mut proto = Vec::new();
        proto.extend_from_slice(&varint_field(1, 10016));
        proto.extend_from_slice(&float_field(2, 1_234_000.0));
        proto.extend_from_slice(&float_field(3, 1_234_500.0));
        proto.extend_from_slice(&varint_field(4, 11));

        let events = detect_mora_events(&build_command(&proto));
        assert_eq!(events.len(), 1);
        match &events[0] {
            MoraEvent::PropChange { old, new, prop_reason, .. } => {
                assert_eq!(*old as i64, 1_234_000);
                assert_eq!(*new as i64, 1_234_500);
                assert_eq!(*prop_reason, Some(11)); // FINISH_QUEST
            }
            other => panic!("expected PropChange, got {other:?}"),
        }
    }

    #[test]
    fn detects_prop_change_2x_layout() {
        // reason=2, prop_type=5 (10016), old=10 (float), cur=11 (float)
        let mut proto = Vec::new();
        proto.extend_from_slice(&varint_field(2, 11));
        proto.extend_from_slice(&varint_field(5, 10016));
        proto.extend_from_slice(&float_field(10, 500.0));
        proto.extend_from_slice(&float_field(11, 900.0));

        let events = detect_mora_events(&build_command(&proto));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].delta(), Some(400));
    }

    #[test]
    fn detects_item_add() {
        // ItemParam {item_id=202, count=1250} on field 1, reason=37 (MONSTER_DIE) on field 3
        let mut param = Vec::new();
        param.extend_from_slice(&varint_field(1, 202));
        param.extend_from_slice(&varint_field(2, 1250));

        let mut proto = Vec::new();
        proto.extend_from_slice(&bytes_field(1, &param));
        proto.extend_from_slice(&varint_field(3, 37));

        let events = detect_mora_events(&build_command(&proto));
        assert_eq!(events.len(), 1);
        match &events[0] {
            MoraEvent::ItemAdd { count, action_reason, .. } => {
                assert_eq!(*count, 1250);
                assert_eq!(*action_reason, Some(37));
            }
            other => panic!("expected ItemAdd, got {other:?}"),
        }
    }

    #[test]
    fn detects_balance_snapshot() {
        // PropValue {type=10016, ival=999999} inside map entry {1: 10016, 2: <PropValue>}
        let mut prop_value = Vec::new();
        prop_value.extend_from_slice(&varint_field(1, 10016));
        prop_value.extend_from_slice(&varint_field(2, 999_999));

        let mut entry = Vec::new();
        entry.extend_from_slice(&varint_field(1, 10016));
        entry.extend_from_slice(&bytes_field(2, &prop_value));

        let mut proto = Vec::new();
        proto.extend_from_slice(&bytes_field(1, &entry));

        let events = detect_mora_events(&build_command(&proto));
        assert!(events.iter().any(|e| matches!(e, MoraEvent::Balance { balance, .. } if *balance as i64 == 999_999)));
    }

    #[test]
    fn ignores_stamina_and_noise() {
        // A stamina-ish change (prop 10011) must not register.
        let mut proto = Vec::new();
        proto.extend_from_slice(&varint_field(1, 10011));
        proto.extend_from_slice(&float_field(2, 22_000.0));
        proto.extend_from_slice(&float_field(3, 21_850.0));
        proto.extend_from_slice(&varint_field(4, 2));

        let events = detect_mora_events(&build_command(&proto));
        assert!(events.is_empty());
    }
}
