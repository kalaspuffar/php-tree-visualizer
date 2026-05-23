//! The 32-character lowercase-hex identifier for a trace.
//!
//! See `SPECIFICATION.md` §4.1.1 for the canonical definition. Two
//! cases:
//!
//! - **Real UUID**: `meta.trace_id` is e.g.
//!   `"01890d1c-12cd-7c0a-9d4b-5e2f6a3b8c19"`. Strip the four
//!   hyphens to yield the 32-char "simple" form.
//! - **All-zero placeholder**: today's extension emits
//!   `"00000000-0000-0000-0000-000000000000"` while UUID v7
//!   generation is still upstream-pending. Synthesise the key
//!   from `SHA-256(host || pid_le || start_time_le)`, take the
//!   first 16 bytes, hex-encode.

use sha2::{Digest, Sha256};

use crate::wire::Meta;

const ALL_ZERO_TRACE_ID: &str = "00000000-0000-0000-0000-000000000000";

/// 32 lowercase hex characters identifying a trace. Filename-safe,
/// URL-safe, deterministic from the meta block.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TraceKey(String);

impl TraceKey {
    /// The 32-character hex stem.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl AsRef<str> for TraceKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TraceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Compute the trace key from a decoded meta block.
pub fn from_meta(meta: &Meta) -> TraceKey {
    // The all-zero placeholder is the only string that maps to the
    // synthesis path; anything else is treated as a real UUID.
    if meta.trace_id == ALL_ZERO_TRACE_ID {
        TraceKey(synthesize(meta))
    } else if let Some(stem) = real_uuid_stem(&meta.trace_id) {
        TraceKey(stem)
    } else {
        // Malformed UUID string from the extension — defensively
        // synthesize so we always produce a 32-hex key. This is a
        // backstop; the wire spec promises UUID-shaped strings.
        TraceKey(synthesize(meta))
    }
}

fn synthesize(meta: &Meta) -> String {
    let mut hasher = Sha256::new();
    hasher.update(meta.host.as_bytes());
    hasher.update(meta.pid.to_le_bytes());
    hasher.update(meta.start_time.to_le_bytes());
    let digest = hasher.finalize();
    hex_encode(&digest[..16])
}

/// Try to interpret `s` as a 36-char hyphenated UUID. Returns the
/// hyphen-stripped 32-char lowercase-hex stem on success, or
/// `None` if the shape doesn't match.
fn real_uuid_stem(s: &str) -> Option<String> {
    if s.len() != 36 {
        return None;
    }
    let bytes = s.as_bytes();
    // Standard UUID has hyphens at offsets 8, 13, 18, 23.
    if bytes[8] != b'-' || bytes[13] != b'-' || bytes[18] != b'-' || bytes[23] != b'-' {
        return None;
    }
    let mut out = String::with_capacity(32);
    for (i, b) in bytes.iter().enumerate() {
        if i == 8 || i == 13 || i == 18 || i == 23 {
            continue;
        }
        if !b.is_ascii_hexdigit() {
            return None;
        }
        // Lowercase the byte. Safe for ASCII hex digits.
        out.push((b.to_ascii_lowercase()) as char);
    }
    Some(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Meta;

    fn meta_with(trace_id: &str, host: &str, pid: u64, start_time: i64) -> Meta {
        Meta {
            schema_version: 1,
            trace_id: trace_id.into(),
            host: host.into(),
            pid,
            start_time,
            sapi: "cli".into(),
            uri_or_script: "x".into(),
            dropped_records: 0,
        }
    }

    #[test]
    fn real_uuid_strips_hyphens_and_lowercases() {
        let m = meta_with("01890d1c-12cd-7c0a-9d4b-5e2f6a3b8c19", "h", 1, 1);
        assert_eq!(from_meta(&m).as_str(), "01890d1c12cd7c0a9d4b5e2f6a3b8c19",);
    }

    #[test]
    fn uppercase_uuid_is_lowercased() {
        let m = meta_with("01890D1C-12CD-7C0A-9D4B-5E2F6A3B8C19", "h", 1, 1);
        assert_eq!(from_meta(&m).as_str(), "01890d1c12cd7c0a9d4b5e2f6a3b8c19",);
    }

    #[test]
    fn all_zero_uuid_triggers_synthesis() {
        let m = meta_with(ALL_ZERO_TRACE_ID, "dev-1", 12345, 1_700_000_000_000_000_000);
        let key = from_meta(&m);
        // 32 lowercase hex
        assert_eq!(key.as_str().len(), 32);
        assert!(key
            .as_str()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn synthesis_is_deterministic_for_the_same_inputs() {
        let m1 = meta_with(ALL_ZERO_TRACE_ID, "dev-1", 12345, 1_700_000_000_000_000_000);
        let m2 = meta_with(ALL_ZERO_TRACE_ID, "dev-1", 12345, 1_700_000_000_000_000_000);
        assert_eq!(from_meta(&m1), from_meta(&m2));
    }

    #[test]
    fn different_host_yields_different_synth_key() {
        let m1 = meta_with(ALL_ZERO_TRACE_ID, "host-a", 1, 1);
        let m2 = meta_with(ALL_ZERO_TRACE_ID, "host-b", 1, 1);
        assert_ne!(from_meta(&m1), from_meta(&m2));
    }

    #[test]
    fn different_pid_yields_different_synth_key() {
        let m1 = meta_with(ALL_ZERO_TRACE_ID, "h", 1, 1);
        let m2 = meta_with(ALL_ZERO_TRACE_ID, "h", 2, 1);
        assert_ne!(from_meta(&m1), from_meta(&m2));
    }

    #[test]
    fn different_start_time_yields_different_synth_key() {
        let m1 = meta_with(ALL_ZERO_TRACE_ID, "h", 1, 1);
        let m2 = meta_with(ALL_ZERO_TRACE_ID, "h", 1, 2);
        assert_ne!(from_meta(&m1), from_meta(&m2));
    }

    #[test]
    fn real_uuid_is_independent_of_synth_inputs() {
        let m1 = meta_with("01890d1c-12cd-7c0a-9d4b-5e2f6a3b8c19", "host-a", 1, 1);
        let m2 = meta_with(
            "01890d1c-12cd-7c0a-9d4b-5e2f6a3b8c19",
            "host-b",
            9999,
            99999,
        );
        // host/pid/start_time vary, but the real UUID branch ignores them
        assert_eq!(from_meta(&m1), from_meta(&m2));
    }

    #[test]
    fn malformed_uuid_falls_back_to_synthesis() {
        let m = meta_with("not-a-uuid", "h", 1, 1);
        let key = from_meta(&m);
        assert_eq!(key.as_str().len(), 32);
        // Deterministic with the synth inputs.
        assert_eq!(key, from_meta(&meta_with("garbage", "h", 1, 1)));
    }

    #[test]
    fn real_uuid_stem_rejects_short_string() {
        assert!(real_uuid_stem("too-short").is_none());
        assert!(real_uuid_stem("").is_none());
    }

    #[test]
    fn real_uuid_stem_rejects_wrong_hyphen_positions() {
        // Same length, but no hyphens.
        assert!(real_uuid_stem("01890d1c12cd7c0a9d4b5e2f6a3b8c19----").is_none());
    }

    #[test]
    fn real_uuid_stem_rejects_non_hex_payload() {
        // Hyphens in the right places but content isn't hex.
        assert!(real_uuid_stem("zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz").is_none());
    }

    #[test]
    fn hex_encode_matches_expected_alphabet() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x10, 0xab]), "00ff10ab");
    }
}
