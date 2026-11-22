//! The stored key layout (plan 0026, "Key layout"; plan 0030).
//!
//! Every key that the server stores starts with one namespace byte:
//!
//! ```text
//! 0x00 ‖ name       system keyspace: server-owned state
//! 0x01 ‖ user key   user keyspace: everything clients write
//! 0x02 .. 0xFF      reserved
//! ```
//!
//! Clients never see the byte. Only replicated state that must roll back
//! with the data belongs in the system keyspace (server roadmap, "The
//! system keyspace").

/// The first byte of every system key.
pub const SYSTEM: u8 = 0x00;
/// The first byte of every user key.
pub const USER: u8 = 0x01;

/// `0x00 ‖ "format"`: the key-layout version that wrote this database.
pub const FORMAT_KEY: &[u8] = b"\x00format";
/// The current key-layout version.
pub const FORMAT_VERSION: &[u8] = &[1];

/// `0x00 ‖ "raft/applied"`: the last applied Raft log id, written with
/// every applied entry ([`encode_log_id`]).
pub const APPLIED_KEY: &[u8] = b"\x00raft/applied";
/// `0x00 ‖ "raft/membership"`: the last applied membership and its log id.
/// The value's membership part is `server-raft`'s encoding.
pub const MEMBERSHIP_KEY: &[u8] = b"\x00raft/membership";

/// A user key must not be empty, as in the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyKey;

/// The stored key for a user key: `0x01 ‖ key`.
pub fn user_key(key: &[u8]) -> Result<Vec<u8>, EmptyKey> {
    if key.is_empty() {
        return Err(EmptyKey);
    }
    Ok([&[USER], key].concat())
}

/// The stored range for a user range `[start, end)`. An unset bound
/// becomes the edge of the user keyspace, so the result never reaches a
/// system key or a reserved prefix.
pub fn user_range(start: Option<&[u8]>, end: Option<&[u8]>) -> (Vec<u8>, Vec<u8>) {
    let start = start.map_or_else(|| vec![USER], |start| [&[USER], start].concat());
    let end = end.map_or_else(|| vec![USER + 1], |end| [&[USER], end].concat());
    (start, end)
}

/// The user key for a stored user key: strip the namespace byte.
pub fn strip(stored: &[u8]) -> Vec<u8> {
    stored[1..].to_vec()
}

/// A log id's value: `term u64 | index u64`, little-endian.
pub fn encode_log_id(term: u64, index: u64) -> [u8; 16] {
    let mut out = [0; 16];
    out[..8].copy_from_slice(&term.to_le_bytes());
    out[8..].copy_from_slice(&index.to_le_bytes());
    out
}

/// The `(term, index)` of a log id's value, or `None` if it is not one.
pub fn decode_log_id(bytes: &[u8]) -> Option<(u64, u64)> {
    let bytes: &[u8; 16] = bytes.try_into().ok()?;
    let (term, index) = bytes.split_at(8);
    Some((
        u64::from_le_bytes(term.try_into().ok()?),
        u64::from_le_bytes(index.try_into().ok()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_keys_take_the_user_prefix() {
        assert_eq!(user_key(b"k").unwrap(), b"\x01k");
        assert_eq!(user_key(b"\x00").unwrap(), b"\x01\x00");
        assert_eq!(user_key(b""), Err(EmptyKey));
        assert_eq!(FORMAT_KEY[0], SYSTEM);
    }

    #[test]
    fn user_ranges_stay_inside_the_user_keyspace() {
        assert_eq!(user_range(None, None), (vec![USER], vec![USER + 1]));
        assert_eq!(
            user_range(Some(b"a"), Some(b"\xff\xff")),
            (b"\x01a".to_vec(), b"\x01\xff\xff".to_vec())
        );
        let (start, _) = user_range(None, None);
        for key in [FORMAT_KEY, APPLIED_KEY, MEMBERSHIP_KEY] {
            assert!(key < start.as_slice(), "system keys sort below user keys");
        }
        assert_eq!(strip(b"\x01abc"), b"abc");
    }

    /// The bytes plan 0028's `ModelSm` wrote must not change.
    #[test]
    fn system_key_bytes_are_fixed() {
        assert_eq!(APPLIED_KEY, b"\x00raft/applied");
        assert_eq!(MEMBERSHIP_KEY, b"\x00raft/membership");
        let value = encode_log_id(3, 0x0102);
        assert_eq!(value, [3, 0, 0, 0, 0, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_log_id(&value), Some((3, 0x0102)));
        assert_eq!(decode_log_id(&value[..15]), None);
    }
}
