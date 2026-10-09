//! Packet capture via Windows' built-in pktmon (ETW / Packet Monitor API) —
//! no Npcap required. Mirrors the approach used by irminsul / emmachase's
//! `pktmon` crate.
//!
//! VPN support notes (e.g. Speedify): with a VPN active, the plaintext game
//! traffic only exists on the VPN's virtual adapter, and the Packet Monitor
//! may report those frames as **raw IP** (layer-3 tunnels such as Wintun)
//! instead of Ethernet. Upstream tools feed frames straight into an
//! Ethernet parser and silently lose them. We normalize every variant:
//!
//! - `Ethernet`/`WiFi` → passed through as-is
//! - `IP` → wrapped in a synthetic Ethernet header (auto-artifactarium only
//!   reads ethertype + IP + UDP ports; dummy MACs are fine)
//! - `Unknown` → auto-sniffed (IPv4/IPv6 or Ethernet by ethertype)
//! - L4-only variants (`UDP`/`TCP`/`L4Payload`/…) lack the IP+UDP headers,
//!   so ports — and therefore traffic direction — cannot be recovered;
//!   they are skipped and counted.
//!
//! Run with `--stats` to see per-component packet counts, which is the
//! quickest way to diagnose VPN/adapter visibility issues.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use pktmon::filter::{PktMonFilter, TransportProtocol};
use pktmon::{Capture, Packet, PacketPayload};
use tokio::sync::mpsc;

/// Genshin Impact game traffic ports.
pub const GAME_PORTS: [u16; 2] = [22101, 22102];

const ETHERTYPE_IPV4: [u8; 2] = [0x08, 0x00];
const ETHERTYPE_IPV6: [u8; 2] = [0x86, 0xDD];

/// Duplicate-packet suppression window: pktmon delivers each packet once per
/// capture component (up to ~5 copies). Dropping byte-identical frames within
/// this window keeps the state machine sane without affecting KCP recovery
/// (its retransmit timers are longer).
const DEDUPE_WINDOW: Duration = Duration::from_millis(120);
const DEDUPE_CACHE: usize = 512;

/// Start capturing UDP frames on the game ports and stream them to the
/// returned channel. Each item is a full Ethernet frame, ready to feed to
/// `GameSniffer::receive_packet`.
///
/// When `stats` is enabled, per-component diagnostics are printed every
/// 15 seconds (useful for verifying capture works through a VPN).
pub fn start_capture(stats: bool) -> Result<mpsc::UnboundedReceiver<Vec<u8>>> {
    let mut capture = Capture::new().context("starting pktmon capture (are you running as admin?)")?;

    for port in GAME_PORTS {
        let filter = PktMonFilter {
            name: format!("GenshinReader UDP {port}"),
            transport_protocol: Some(TransportProtocol::UDP),
            port: port.into(),
            ..PktMonFilter::default()
        };
        capture
            .add_filter(filter)
            .with_context(|| format!("adding pktmon filter for port {port}"))?;
    }

    let stream = capture
        .stream()
        .context("opening pktmon stream")?
        .boxed();

    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let mut stream = stream;
        let mut counters = CaptureCounters::default();
        let mut last_report = Instant::now();
        let mut recent: VecDeque<(u64, Instant)> = VecDeque::new();

        loop {
            match tokio::time::timeout(Duration::from_secs(1), stream.next()).await {
                Ok(Some(packet)) => {
                    counters.total += 1;
                    *counters
                        .by_component
                        .entry(packet.component_id)
                        .or_default() += 1;

                    let Some(frame) = normalize_packet(packet, &mut counters) else {
                        counters.skipped += 1;
                        continue;
                    };

                    // Suppress duplicate deliveries of the same frame (one
                    // per capture component) within a short window.
                    let hash = fnv1a(&frame);
                    let now = Instant::now();
                    recent.retain(|(_, seen)| now.duration_since(*seen) < DEDUPE_WINDOW);
                    if recent.iter().any(|(h, _)| *h == hash) {
                        counters.deduped += 1;
                        continue;
                    }
                    recent.push_back((hash, now));
                    if recent.len() > DEDUPE_CACHE {
                        recent.pop_front();
                    }

                    counters.usable += 1;
                    if tx.send(frame).is_err() {
                        break; // receiver dropped; stop capturing
                    }
                }
                Ok(None) => {
                    tracing::warn!("pktmon stream ended");
                    break;
                }
                Err(_) => {
                    // Momentary silence is normal; check whether the receiver is gone.
                    if tx.is_closed() {
                        break;
                    }
                }
            }

            if stats && last_report.elapsed() >= Duration::from_secs(15) {
                counters.report();
                last_report = Instant::now();
            }
        }
    });

    Ok(rx)
}

/// FNV-1a hash for duplicate detection (not cryptographic — just fast).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Convert a captured packet into an Ethernet frame the sniffer can parse,
/// regardless of the framing the Packet Monitor reported.
fn normalize_packet(packet: Packet, counters: &mut CaptureCounters) -> Option<Vec<u8>> {
    let Packet { payload, .. } = packet;
    match payload {
        PacketPayload::Ethernet(frame) | PacketPayload::WiFi(frame) => Some(frame),
        PacketPayload::IP(frame) => Some(wrap_raw_ip(&frame, counters)),
        PacketPayload::Unknown(frame) => sniff_frame(&frame, counters),
        // L4-only and other payloads lack (or are not) IP+UDP headers, so the
        // game ports — and traffic direction — cannot be recovered.
        PacketPayload::UDP(_)
        | PacketPayload::TCP(_)
        | PacketPayload::HTTP(_)
        | PacketPayload::L4Payload(_)
        | PacketPayload::ARP(_)
        | PacketPayload::ICMP(_)
        | PacketPayload::ESP(_)
        | PacketPayload::AH(_) => None,
    }
}

/// Prepend a synthetic Ethernet header to a raw IP packet (layer-3 tunnel
/// adapters such as Wintun deliver frames this way).
fn wrap_raw_ip(ip: &[u8], counters: &mut CaptureCounters) -> Vec<u8> {
    counters.from_raw_ip += 1;
    let ethertype = match ip.first().map(|b| b >> 4) {
        Some(4) => ETHERTYPE_IPV4,
        Some(6) => ETHERTYPE_IPV6,
        // Not actually IP; pass through and let the parser drop it.
        _ => [0x00, 0x00],
    };
    let mut frame = Vec::with_capacity(14 + ip.len());
    frame.extend_from_slice(&[0; 12]); // dummy MACs (unused by the sniffer)
    frame.extend_from_slice(&ethertype);
    frame.extend_from_slice(ip);
    frame
}

/// Guess the framing of an `Unknown` payload.
fn sniff_frame(bytes: &[u8], counters: &mut CaptureCounters) -> Option<Vec<u8>> {
    if bytes.len() >= 20 && matches!(bytes.first().map(|b| b >> 4), Some(4) | Some(6)) {
        return Some(wrap_raw_ip(bytes, counters));
    }
    // Ethernet: sensible-looking ethertype at offset 12.
    if bytes.len() >= 14 {
        let ethertype = [bytes[12], bytes[13]];
        if ethertype == ETHERTYPE_IPV4 || ethertype == ETHERTYPE_IPV6 || ethertype == [0x81, 0x00] {
            counters.from_ethernet_sniffed += 1;
            return Some(bytes.to_vec());
        }
    }
    None
}

#[derive(Default)]
struct CaptureCounters {
    total: u64,
    usable: u64,
    skipped: u64,
    deduped: u64,
    from_raw_ip: u64,
    from_ethernet_sniffed: u64,
    by_component: HashMap<u16, u64>,
}

impl CaptureCounters {
    fn report(&self) {
        let mut components: Vec<_> = self.by_component.iter().collect();
        components.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        let component_list = components
            .iter()
            .map(|(id, count)| format!("component {id}: {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "[stats] packets: {} (usable {}, raw-IP wrapped {}, deduped {}, skipped {}) | {component_list}",
            self.total, self.usable, self.from_raw_ip, self.deduped, self.skipped
        );
        if self.skipped > 0 {
            println!(
                "[stats] note: {} packets were skipped (L4-only framing). If game traffic is missing while a VPN runs, this matters.",
                self.skipped
            );
        }
    }
}
