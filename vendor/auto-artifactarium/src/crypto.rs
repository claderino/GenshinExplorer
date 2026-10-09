use std::collections::HashMap;

use rand_mt::Mt64;
use tracing::{debug, info, instrument, trace, warn};

use crate::bytes_as_hex;
use crate::cs_rand::Random;

#[instrument(skip_all)]
pub fn decrypt_command(key: &[u8], encrypted: &mut [u8]) {
    trace!(data = bytes_as_hex(encrypted), "before decryption");

    for i in 0..encrypted.len() {
        encrypted[i] ^= key[i % key.len()];
    }

    trace!(data = bytes_as_hex(encrypted), "after decryption");
}

pub fn lookup_initial_key(initial_keys: &HashMap<u16, Vec<u8>>, bytes: &[u8]) -> Option<Vec<u8>> {
    let version = u16::from_be_bytes(bytes[..2].try_into().unwrap()) ^ 0x4567;

    // attempt to fetch from user provided initial keys, otherwise use our own baked-in ones
    let key = initial_keys.get(&version).cloned();
    match key {
        Some(key) => {
            info!(version, "found initial decryption key");
            Some(key)
        }
        None => {
            info!(version, "didn't find decryption key");
            None
        }
    }
}

pub fn new_key_from_seed(seed: u64) -> Vec<u8> {
    // mersenne twister generator
    let mut first = Mt64::new(seed);
    let mut generator = Mt64::new(first.next_u64());

    let _ = generator.next_u64(); // skip first number

    let mut key = Vec::with_capacity(512);
    for _ in 0..512 {
        for b in generator.next_u64().to_be_bytes() {
            key.push(b);
        }
    }
    key
}

pub fn guess(seed: i64, server_seed: u64, depth: i32, data: Vec<u8>) -> Option<Vec<u8>> {
    // Attempt to generate the key.
    let mut generator = Random::seeded(seed as i32);
    for _ in 0..depth {
        let client_seed = generator.next_safe_uint64();

        let seed = client_seed ^ server_seed;
        let key = new_key_from_seed(seed);

        let mut clone = data.clone();
        decrypt_command(&key, &mut clone);

        if clone[0] == 0x45
            && clone[1] == 0x67
            && clone[clone.len() - 2] == 0x89
            && clone[clone.len() - 1] == 0xAB
        {
            debug!("Found encryption key seed: {seed}");
            return Some(key);
        }
    }

    None
}

pub fn bruteforce(sent_time: u64, server_seed: u64, data: Vec<u8>) -> Option<(u64, Vec<u8>)> {
    debug!("Running bruteforce loop.");
    // Generate new seeds.
    for i in 0..3000i64 {
        let offset = if i % 2 == 0 { i / 2 } else { -(i - 1) / 2 };
        let time = sent_time as i64 + offset; // This will act as the seed.

        if let Some(key) = guess(time, server_seed, 5, data.clone()) {
            return Some((time as u64, key));
        }
    }
    warn!("Unable to find the encryption key seed.");
    None
}

/// GenshinReader patch: some game servers report `sent_ms` with a timezone
/// offset baked into the epoch (observed: exactly +3600000 ms off true UTC).
/// The stock bruteforce only scans ±1.5 s around that value, so the real
/// client seed can sit hours outside the search window and the session key
/// is never recovered.
///
/// This sweep retries the stock ±1.5 s window around a series of shifted
/// centers: the local wall clock, and the reported time shifted by every
/// 15-minute step up to ±14 hours (covers all timezone offsets, including
/// half- and quarter-hour ones). Costs a few seconds once per login and is
/// cached against repeated failures by the caller.
pub fn bruteforce_extended(center_ms: u64, server_seed: u64, data: &[u8]) -> Option<(u64, Vec<u8>)> {
    let mut centers: Vec<i64> = Vec::with_capacity(116);

    // True epoch now — used when the client derived its seed from UTC.
    if let Ok(elapsed) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        centers.push(elapsed.as_millis() as i64);
    }

    // Timezone-shifted centers around the reported time.
    for k in 1i64..=56 {
        let step = k * 900_000; // 15 minutes
        centers.push(center_ms as i64 + step);
        centers.push(center_ms as i64 - step);
    }

    for center in centers {
        if center <= 0 {
            continue;
        }
        if let Some(found) = bruteforce(center as u64, server_seed, data.to_vec()) {
            info!(center, "found key via extended timezone sweep");
            return Some(found);
        }
    }

    warn!("Extended key sweep exhausted without a match.");
    None
}
