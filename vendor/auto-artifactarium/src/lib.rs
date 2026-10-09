//! Parse network packets transmitted between the game and the server
//!
//! Packets are built up in following layers depending on the purpose of the packet:
//!
//! - Packets for connection management ([`GamePacket::Connection`])
//!     - **Ethernet/IP/UDP**, handled using [`etherparse`]
//!     - **[`ConnectionPacket`]**, containing events for connection establishment/disconnection
//! - Packets for game commands ([`GamePacket::Commands`])
//!     - **Ethernet/IP/UDP**, handled using [`etherparse`]
//!     - **KCP**, handled using [`kcp`]
//!         - The KCP header contains an extra field that needs to be removed
//!           to be compatible with the regular KCP protocol
//!     - **[`GameCommand`]**, encrypted using XOR
//!     - **Protobuf**, payload, needs to be parsed into using the types generated in [`gen::proto`]
//!
//! [`GameCommand`]s are encrypted using an XOR-key.
//! One of the first packets sent is a request for a new key from a seed.
//! That key is used for the rest of the packets.
//! This means the recording for packets needs to start before the game starts (train hyperdrive).
//!
//! ## Example
//! ```
//! use auto_artifactarium::{GamePacket, GameSniffer, ConnectionPacket};
//!
//! let packets: Vec<Vec<u8>> = vec![/**/];
//!
//! let mut sniffer = GameSniffer::new();
//! for packet in packets {
//!     match sniffer.receive_packet(packet) {
//!         Some(GamePacket::Connection(ConnectionPacket::Disconnected)) => {
//!             println!("Disconnected!");
//!             break;
//!         }
//!         Some(GamePacket::Commands(commands)) => {
//!             for command in commands {
//!                 println!("{:?}", command);
//!             }
//!         }
//!         _ => {}
//!     }
//! }
//! ```
//!

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write;

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use protobuf::Message;
use rsa::{RsaPrivateKey, pkcs1::DecodeRsaPrivateKey};
use tracing::{error, info, info_span, instrument, trace, warn};

use crate::connection::parse_connection_packet;
use crate::crypto::{bruteforce, bruteforce_extended, decrypt_command, lookup_initial_key};
// use crate::gen::protos::GetPlayerTokenRsp;
use crate::Key::Dispatch;
use crate::r#gen::protos::PacketHead;
use crate::kcp::KcpSniffer;
pub use crate::unk_util::Achievement;
pub use crate::unk_util::{
    matches_achievement_all_data_notify, matches_avatars_all_data_notify,
    matches_get_player_token_rsp, matches_items_all_data_notify,
};

fn bytes_as_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, b| {
        let _ = write!(output, "{b:02x}");
        output
    })
}

// pub mod command_id;
pub mod r#gen;

mod connection;
mod crypto;
mod cs_rand;
mod kcp;
mod unk_util;

const PORTS: [u16; 2] = [22101, 22102];

/// Top-level packet sent by the game
pub enum GamePacket {
    Connection(ConnectionPacket),
    Commands(Vec<GameCommand>),
}

/// Packet for connection management
pub enum ConnectionPacket {
    HandshakeRequested,
    Disconnected,
    HandshakeEstablished,
    SegmentData(PacketDirection, Vec<u8>),
}

#[repr(u16)]
enum CommandId {
    AvatarDataNotify =  27799,
    PlayerStoreNotify = 22160,
}

/// Game command header.
///
/// Contains the type of the command in `command_id`
/// and the data encoded in protobuf in `proto_data`
///
/// ## Bit Layout
/// | Bit indices     |  Type |  Name |
/// | - | - | - |
/// |   0..2      |  `u16`  |  Header (magic constant) |
/// |   2..4      |  `u16`  |  command_id |
/// |   4..6      |  `u16`  |  header_len (unsure) |
/// |   6..10     |  `u32`  |  data_len |
/// |  10..10+data_len |  variable  |  proto_data |
/// | data_len..data_len+2  |  `u16`  |  Tail (magic constant) |
#[derive(Clone)]
pub struct GameCommand {
    pub command_id: u16,
    #[allow(unused)]
    pub header_len: u16,
    #[allow(unused)]
    pub data_len: u32,
    /// The protobuf PacketHead bytes (ported from upstream hashblen
    /// 878e7a4 — previously the header and body were concatenated, letting
    /// body fields clobber header fields during PacketHead parsing).
    #[allow(unused)]
    pub proto_header: Vec<u8>,
    pub proto_data: Vec<u8>,
}

impl GameCommand {
    const HEADER_LEN: usize = 10;
    const TAIL_LEN: usize = 2;

    #[instrument(skip(bytes), fields(len = bytes.len()))]
    pub fn try_new(bytes: Vec<u8>) -> Option<Self> {
        let header_overhead = Self::HEADER_LEN + Self::TAIL_LEN;
        if bytes.len() < header_overhead {
            warn!(len = bytes.len(), "game command header incomplete");
            return None;
        }

        if bytes[0] != 0x45
            || bytes[1] != 0x67
            || bytes[bytes.len() - 2] != 0x89
            || bytes[bytes.len() - 1] != 0xAB
        {
            error!("Didn't get magic in try_new!");
            return None;
        }

        // skip header magic const
        let command_id = u16::from_be_bytes(bytes[2..4].try_into().unwrap());
        let header_len = u16::from_be_bytes(bytes[4..6].try_into().unwrap());
        let data_len = u32::from_be_bytes(bytes[6..10].try_into().unwrap());

        // Ported from upstream hashblen 878e7a4: separate the PacketHead
        // from the body instead of concatenating them.
        let data_start = 10 + header_len as usize;
        let data_end = data_start + data_len as usize;
        if data_end > bytes.len() {
            warn!(len = bytes.len(), "game command buffer too short");
            return None;
        }

        let proto_header = bytes[10..data_start].to_vec();
        let proto_data = bytes[data_start..data_end].to_vec();
        Some(GameCommand {
            command_id,
            header_len,
            data_len,
            proto_header,
            proto_data,
        })
    }

    pub fn parse_proto<T: protobuf::Message>(&self) -> protobuf::Result<T> {
        T::parse_from_bytes(&self.proto_data)
    }

    pub fn is_avatar_data_notify(&self) -> bool {
        self.command_id == CommandId::AvatarDataNotify as u16
    }

    pub fn is_player_store_notify(&self) -> bool {
        self.command_id == CommandId::PlayerStoreNotify as u16
    }
}

impl fmt::Debug for GameCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GameCommand")
            .field("command_id", &self.command_id)
            .field("header_len", &self.header_len)
            .field("data_len", &self.data_len)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum PacketDirection {
    Sent,
    Received,
}

pub enum Key {
    Dispatch(Vec<u8>),
    Session(Vec<u8>),
}

#[derive(Default)]
pub struct GameSniffer {
    sent_kcp: Option<KcpSniffer>,
    recv_kcp: Option<KcpSniffer>,
    client_seed: Option<u64>,
    key: Option<Key>,
    initial_keys: HashMap<u16, Vec<u8>>,
    rsa_keys: Vec<RsaPrivateKey>,
    sent_time: Option<u64>,
    possible_seeds: Vec<u64>,
    // GenshinReader patches:
    /// Last handshake reset (used to debounce duplicated handshake packets
    /// delivered once per capture component).
    last_handshake_reset: Option<std::time::Instant>,
    /// Consecutive conv mismatches per direction (used to adopt a new
    /// conversation after a relogin / account switch).
    sent_conv_mismatches: u32,
    recv_conv_mismatches: u32,
    /// Account id observed in the login token response (ASCII digits);
    /// 7.x PacketHead.user_id is not populated, so this is the reliable
    /// per-account marker.
    account_uid: Option<u32>,
    /// Bounded counter of dispatch-phase commands dumped for protocol
    /// analysis (cold-start vs relogin diffing).
    dispatch_dumped: u32,
    /// Exhausted key searches (sent_time, seed) — avoids re-running the
    /// expensive extended sweep for every subsequent packet of a login
    /// that fundamentally cannot be decrypted.
    failed_key_search: Option<(u64, u64)>,
}

impl GameSniffer {
    pub fn new() -> Self {
        let pem_data_4 = include_str!("../keys/private_key_4.pem");
        let pem_data_5 = include_str!("../keys/private_key_5.pem");

        let rsa_4 = RsaPrivateKey::from_pkcs1_pem(pem_data_4);
        let rsa_5 = RsaPrivateKey::from_pkcs1_pem(pem_data_5);

        GameSniffer {
            rsa_keys: vec![rsa_4, rsa_5]
                .iter()
                .filter_map(|rsa_key| rsa_key.clone().ok())
                .collect(),
            ..Default::default()
        }
    }

    pub fn set_initial_keys(mut self, initial_keys: HashMap<u16, Vec<u8>>) -> Self {
        self.initial_keys = initial_keys;
        self
    }

    /// Account id observed in the login token response (GenshinReader
    /// extension; stable per account, used for per-UID ledger separation).
    pub fn account_uid(&self) -> Option<u32> {
        self.account_uid
    }

    #[instrument(skip_all, fields(len = bytes.len()))]
    pub fn receive_packet(&mut self, bytes: Vec<u8>) -> Option<GamePacket> {
        let packet = parse_connection_packet(&PORTS, bytes)?;
        match packet {
            ConnectionPacket::HandshakeRequested => {
                // GenshinReader patch: packet capture via pktmon delivers each
                // handshake packet once per capture component (up to ~5 copies
                // within milliseconds). Each copy used to wipe the KCP
                // sniffers mid-login; debounce so only the first copy within
                // a short window performs the reset.
                let now = std::time::Instant::now();
                let debounced = self
                    .last_handshake_reset
                    .is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(1));
                if debounced {
                    trace!("ignoring duplicate handshake request");
                    return None;
                }
                self.last_handshake_reset = Some(now);

                info!("handshake requested, resetting state");
                self.recv_kcp = None;
                self.sent_kcp = None;
                self.key = None;
                self.sent_conv_mismatches = 0;
                self.recv_conv_mismatches = 0;
                self.failed_key_search = None;
                self.account_uid = None;
                self.dispatch_dumped = 0;
                Some(GamePacket::Connection(packet))
            }
            ConnectionPacket::HandshakeEstablished | ConnectionPacket::Disconnected => {
                Some(GamePacket::Connection(packet))
            }

            ConnectionPacket::SegmentData(direction, kcp_seg) => {
                let commands = self.receive_kcp_segment(direction, &kcp_seg);
                match commands {
                    Some(commands) => Some(GamePacket::Commands(commands)),
                    None => Some(GamePacket::Connection(ConnectionPacket::SegmentData(
                        direction, kcp_seg,
                    ))),
                }
            }
        }
    }

    fn receive_kcp_segment(
        &mut self,
        direction: PacketDirection,
        kcp_seg: &[u8],
    ) -> Option<Vec<GameCommand>> {
        let (kcp, mismatches) = match direction {
            PacketDirection::Sent => (&mut self.sent_kcp, &mut self.sent_conv_mismatches),
            PacketDirection::Received => (&mut self.recv_kcp, &mut self.recv_conv_mismatches),
        };

        if kcp.is_none() {
            let new_kcp = KcpSniffer::try_new(kcp_seg)?;
            *kcp = Some(new_kcp);
        } else if kcp_seg.len() > 8 {
            // GenshinReader patch: the game switches to a new conversation on
            // every login (relogin, account switch). If a sustained stream of
            // segments carries a different conv id than the one we follow —
            // e.g. stale duplicates made us adopt the wrong conversation —
            // adopt the new conversation instead of dropping its packets
            // forever.
            let incoming = ::kcp::get_conv(kcp_seg);
            let current = kcp.as_ref().expect("kcp sniffer present").conv();
            if incoming != current {
                *mismatches += 1;
                if *mismatches >= 8 {
                    info!(incoming, current, "adopting new kcp conversation");
                    if let Some(new_kcp) = KcpSniffer::try_new(kcp_seg) {
                        *kcp = Some(new_kcp);
                    }
                    *mismatches = 0;
                }
            } else {
                *mismatches = 0;
            }
        }

        if let Some(kcp) = kcp {
            let commands = kcp
                .receive_segments(kcp_seg)
                .into_iter()
                .filter_map(|data| self.receive_command(data))
                .collect();

            return Some(commands);
        }

        None
    }

    #[instrument(skip_all, fields(len = data.len()))]
    fn receive_command(&mut self, mut data: Vec<u8>) -> Option<GameCommand> {
        let key_r = match &self.key {
            None => {
                let key = lookup_initial_key(&self.initial_keys, &data);
                match key {
                    Some(key) => {
                        self.key = Some(Dispatch(key));
                        self.key.as_ref().unwrap()
                    }
                    None => {
                        error!("No dispatch key found");
                        return None;
                    }
                }
            }
            Some(Dispatch(k)) => {
                let mut test = data.clone();
                decrypt_command(k, &mut test);

                if test[0] == 0x45
                    && test[1] == 0x67
                    && test[test.len() - 2] == 0x89
                    && test[test.len() - 1] == 0xAB
                {
                    self.key.as_ref().unwrap()
                } else {
                    let mut discovered_key: Option<&Key> = None;
                    let center = self.sent_time.unwrap_or(0);
                    'seeds: for &seed in &self.possible_seeds {
                        // Skip searches we already know are futile.
                        if self.failed_key_search == Some((center, seed)) {
                            continue;
                        }

                        // First try with a retained client seed.
                        if let Some(client_seed) = self.client_seed
                            && let Some((client_seed, key)) =
                                bruteforce(client_seed, seed, data.clone())
                        {
                            self.client_seed = Some(client_seed);
                            self.key = Some(Key::Session(key));
                            discovered_key = self.key.as_ref();
                            break 'seeds;
                        }

                        // If that fails, try with a client seed generated from the packet's
                        // `sent_time`
                        if let Some((client_seed, key)) =
                            bruteforce(center, seed, data.clone())
                        {
                            self.client_seed = Some(client_seed);
                            self.key = Some(Key::Session(key));
                            discovered_key = self.key.as_ref();
                            break 'seeds;
                        }

                        // GenshinReader patch: the reported sent_time may carry
                        // a baked-in timezone offset (observed +1 h), leaving
                        // the true client seed far outside the ±1.5 s window.
                        // Sweep timezone-shifted centers before giving up.
                        if let Some((client_seed, key)) =
                            bruteforce_extended(center, seed, &data)
                        {
                            self.client_seed = Some(client_seed);
                            self.key = Some(Key::Session(key));
                            discovered_key = self.key.as_ref();
                            break 'seeds;
                        }

                        self.failed_key_search = Some((center, seed));
                        error!(seed, center, "exhausted key search for seed");
                        dump_key_failure(center, seed, &data);
                    }

                    match discovered_key {
                        Some(key) => {
                            self.failed_key_search = None;
                            key
                        }
                        None => {
                            // Cached-futile searches land here on every packet —
                            // keep the log quiet after the first report.
                            trace!("Couldn't bruteforce from deduced keys");
                            return None;
                        }
                    }
                }
            }
            Some(Key::Session(k)) => {
                let mut test = data.clone();
                decrypt_command(k, &mut test);

                if test[0] == 0x45 && test[1] == 0x67 {
                    //|| test[test.len() - 2] == 0x89 && test[test.len() - 1] == 0xAB
                    self.key.as_ref().unwrap()
                } else {
                    warn!("Invalidated session key");
                    self.key = None;
                    error!("Session key dead, relaunch game");
                    return None;
                }
            }
        };

        let key = match key_r {
            Dispatch(k) | Key::Session(k) => k,
        };

        decrypt_command(key, &mut data);

        let command = GameCommand::try_new(data)?;

        let span = info_span!("command", ?command);
        let _enter = span.enter();

        info!("received");
        trace!(data = BASE64_STANDARD.encode(&command.proto_data), "data");

        // if !matches!(
        //     command.command_id,
        //     command_id::GET_PLAYER_TOKEN_RSP | command_id::ACHIEVEMENT_ALL_DATA_NOTIFY
        // ) {
        //     return None;
        // }

        if let Some(Dispatch(_)) = &self.key {
            // GenshinReader forensics: dump the first few dispatch-phase
            // command bodies (requests AND responses) so login flows can be
            // diffed offline (cold start vs relogin).
            if self.dispatch_dumped < 8 {
                self.dispatch_dumped += 1;
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let dir = std::env::var_os("LOCALAPPDATA")
                    .map(std::path::PathBuf::from)
                    .map(|base| base.join("GenshinReader").join("keydump"));
                if let Some(dir) = dir {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(
                        dir.join(format!("dispatch_{}_{ts}.bin", command.command_id)),
                        &command.proto_data,
                    );
                }
            }

            // GenshinReader diagnostics: log the wire header of every
            // dispatch-phase command (packet_id + the sender's timestamp —
            // the client's own requests reveal its true clock). The header
            // is parsed from the separated PacketHead bytes (no body fields
            // can clobber it).
            if let Ok(head) = PacketHead::parse_from_bytes(&command.proto_header) {
                info!(
                    packet_id = head.packet_id,
                    sent_ms = head.sent_ms,
                    command_id = command.command_id,
                    "dispatch-phase command"
                );
            }
            if let Some(possible_seeds) =
                matches_get_player_token_rsp(command.proto_data.clone(), self.rsa_keys.clone())
            {
                self.possible_seeds = possible_seeds;
                info!(?self.possible_seeds, "setting new possible session seeds");
                let header_command = PacketHead::parse_from_bytes(&command.proto_header).unwrap();
                self.sent_time = Some(header_command.sent_ms);
                info!(?self.sent_time, "setting new send time");
                dump_artifacts("token_rsp_decrypted", &command.proto_data);

                // GenshinReader: the token response carries the account id as
                // an ASCII-digit field — 7.x heads no longer populate
                // PacketHead.user_id, so keep this as the account marker.
                if self.account_uid.is_none() {
                    self.account_uid = extract_account_uid(&command.proto_data);
                    if let Some(uid) = self.account_uid {
                        info!(uid, "account id observed in token response");
                    }
                }
            }
        }

        Some(command)
    }
}

pub fn matches_achievement_packet(game_command: &GameCommand) -> Option<Vec<Achievement>> {
    return matches_achievement_all_data_notify(game_command.proto_data.clone());
}

// ---------------------------------------------------------------------------
// GenshinReader forensics dumps: written only when the session key cannot be
// derived, so the encrypted bytes and login material can be analyzed offline
// instead of requiring repeated instrumented logins.
// ---------------------------------------------------------------------------

/// Extracts the account id from a decrypted login token response: the first
/// ASCII-digit run of 6–10 characters (the account uid field, e.g.
/// "14771801"). Returns None if no such run exists.
fn extract_account_uid(body: &[u8]) -> Option<u32> {
    let mut best: Option<&[u8]> = None;
    let mut start: Option<usize> = None;
    for (i, &b) in body.iter().enumerate() {
        let is_digit = b.is_ascii_digit();
        if is_digit && start.is_none() {
            start = Some(i);
        } else if !is_digit {
            if let Some(s) = start.take() {
                let run = &body[s..i];
                if (6..=10).contains(&run.len()) && best.is_none() {
                    best = Some(run);
                }
            }
        }
    }
    if let Some(s) = start {
        let run = &body[s..];
        if (6..=10).contains(&run.len()) && best.is_none() {
            best = Some(run);
        }
    }
    best.and_then(|run| std::str::from_utf8(run).ok())
        .and_then(|s| s.parse::<u32>().ok())
}

fn dump_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .map(|base| base.join("GenshinReader").join("keydump"))?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn dump_artifacts(tag: &str, bytes: &[u8]) {
    let Some(dir) = dump_dir() else { return };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{tag}_{ts}.bin"));
    let _ = std::fs::write(path, bytes);
}

fn dump_key_failure(center: u64, seed: u64, encrypted: &[u8]) {
    let Some(dir) = dump_dir() else { return };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = std::fs::write(dir.join(format!("keyfail_{ts}.bin")), encrypted);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let meta = format!(
        "sent_time_center={center}\nserver_seed={seed}\nwall_epoch_ms={now_ms}\nencrypted_len={}\n",
        encrypted.len()
    );
    let _ = std::fs::write(dir.join(format!("keyfail_{ts}.meta")), meta);
}

pub fn matches_item_packet(game_command: &GameCommand) -> Option<Vec<r#gen::protos::Item>> {
    if !game_command.is_player_store_notify() {
        return None;
    }

    return matches_items_all_data_notify(&game_command.proto_data);
}

pub fn matches_avatar_packet(game_command: &GameCommand) -> Option<Vec<r#gen::protos::AvatarInfo>> {
    if !game_command.is_avatar_data_notify() {
        return None;
    }

    return matches_avatars_all_data_notify(&game_command.proto_data);
}
