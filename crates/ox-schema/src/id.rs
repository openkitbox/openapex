//! Ascending, lexicographically-sortable identifiers compatible with opencode
//! (`packages/schema/src/identifier.ts`).
//!
//! An ID body is 26 characters:
//! - first 12 chars: lowercase hex of the low 48 bits of
//!   `timestamp_millis * 0x1000 + counter` as 6 big-endian bytes (the counter
//!   resets each millisecond),
//! - remaining 14 chars: random chars from the alphabet
//!   `0-9 A-Z a-z` (62 chars), chosen by `byte % 62` exactly like upstream.
//!
//! Public IDs are prefixed, e.g. `ses_...`, `msg_...`, `evt_...`.
//! Because the time value leads the string and truncation to 48 bits is
//! uniform, IDs sort ascending by plain string comparison. (Note the
//! truncation is upstream-faithful: the decodable timestamp is
//! `millis mod 2^36`, roughly a 69-year cycle.)

use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;

/// Length of the unprefixed ID body (upstream `length = 26`).
pub const BODY_LENGTH: usize = 26;
/// Number of leading hex characters encoding timestamp + counter.
const TIME_HEX_CHARS: usize = 12;
/// 48-bit mask applied before hex encoding (mirrors upstream's 6-byte
/// extraction from the BigInt value).
const TIME_MASK: u64 = 0xffff_ffff_ffff;

const CHARS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Last `(timestamp_millis, counter)` pair. A mutex is used rather than CAS
/// loops: contention here is negligible and correctness is trivial to verify.
static STATE: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// Milliseconds since the Unix epoch (0 on clock skew before the epoch).
/// Shared by ID generation and API handlers.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Encode the 12-char hex time portion: low 48 bits of
/// `timestamp * 0x1000 + counter` as 6 big-endian bytes.
fn encode_time(timestamp: u64, counter: u64) -> String {
    // TIME_MASK caps the value at 48 bits, so `{value:012x}` is exactly the
    // 6-byte big-endian hex upstream produces per byte.
    let value = timestamp.wrapping_mul(0x1000).wrapping_add(counter) & TIME_MASK;
    let mut hex = String::with_capacity(TIME_HEX_CHARS);
    write!(hex, "{value:012x}").expect("writing to a String never fails");
    hex
}

/// Generate the 26-char ascending ID body for the current time.
fn ascending_body(timestamp: u64) -> String {
    let counter = {
        let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
        let counter = if timestamp != state.0 { 1 } else { state.1 + 1 };
        *state = (timestamp, counter);
        counter
    };

    let mut body = encode_time(timestamp, counter);

    let mut rng = rand::rng();
    let random: Vec<u8> = (0..(BODY_LENGTH - TIME_HEX_CHARS))
        .map(|_| rng.random::<u8>())
        .collect();
    for byte in random {
        body.push(CHARS[(byte % 62) as usize] as char);
    }
    body
}

/// Generate an ascending ID with the given prefix, e.g. `id("ses")` ->
/// `"ses_019a2b..."`.
pub fn id(prefix: &str) -> String {
    format!("{prefix}_{}", ascending_body(now_millis() as u64))
}

/// Derive a deterministic message ID from a durable event ID by swapping the
/// `evt_` prefix for `msg_` (upstream `SessionMessage.ID.fromEvent`).
///
/// Projected messages derive their ID from the committing event so that
/// replaying the same event reproduces the same message row (replay
/// idempotency). Non-`evt_` inputs are passed through with the prefix swapped
/// only when present; otherwise the body is reused verbatim.
pub fn from_event(event_id: &str) -> String {
    match event_id.strip_prefix("evt_") {
        Some(body) => format!("msg_{body}"),
        None => format!("msg_{event_id}"),
    }
}
