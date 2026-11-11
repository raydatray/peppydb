//! The stored key layout (plan 0026, "Key layout").
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
//! with the data belongs in the system keyspace.

use engine::Db;
use tonic::Status;

/// The first byte of every user key.
pub(crate) const USER: u8 = 0x01;

/// `0x00 ‖ "format"`: the key-layout version that wrote this database.
pub(crate) const FORMAT_KEY: &[u8] = b"\x00format";
/// The current key-layout version.
pub(crate) const FORMAT_VERSION: &[u8] = &[1];

/// The stored key for a user key: `0x01 ‖ key`. An empty user key is
/// `INVALID_ARGUMENT`, as in the engine.
pub(crate) fn user_key(key: &[u8]) -> Result<Vec<u8>, Status> {
    if key.is_empty() {
        return Err(Status::invalid_argument("key must not be empty"));
    }
    Ok([&[USER], key].concat())
}

/// The stored range for a user range `[start, end)`. An unset bound
/// becomes the edge of the user keyspace, so the result never reaches a
/// system key or a reserved prefix.
pub(crate) fn user_range(start: Option<&[u8]>, end: Option<&[u8]>) -> (Vec<u8>, Vec<u8>) {
    let start = start.map_or_else(|| vec![USER], |start| [&[USER], start].concat());
    let end = end.map_or_else(|| vec![USER + 1], |end| [&[USER], end].concat());
    (start, end)
}

/// The user key for a stored user key: strip the namespace byte.
pub(crate) fn strip(stored: &[u8]) -> Vec<u8> {
    stored[1..].to_vec()
}

/// Why a database cannot be served.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("the database holds data but no format marker; it was not written by the server")]
    Missing,
    #[error("the database has format version {0:?}, but this server writes {FORMAT_VERSION:?}")]
    Unsupported(Vec<u8>),
    #[error(transparent)]
    Engine(#[from] engine::Error),
}

/// Check the format marker, or write it into an empty database.
pub(crate) fn check_format(db: &Db) -> Result<(), FormatError> {
    match db.get(FORMAT_KEY)? {
        Some(version) if version == FORMAT_VERSION => Ok(()),
        Some(version) => Err(FormatError::Unsupported(version)),
        None => {
            if db.range(None, None)?.next().is_some() {
                return Err(FormatError::Missing);
            }
            db.put(FORMAT_KEY, FORMAT_VERSION)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_keys_take_the_user_prefix() {
        assert_eq!(user_key(b"k").unwrap(), b"\x01k");
        assert_eq!(user_key(b"\x00").unwrap(), b"\x01\x00");
        assert_eq!(
            user_key(b"").unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert_eq!(FORMAT_KEY[0], 0x00);
    }

    #[test]
    fn user_ranges_stay_inside_the_user_keyspace() {
        assert_eq!(user_range(None, None), (vec![USER], vec![USER + 1]));
        assert_eq!(
            user_range(Some(b"a"), Some(b"\xff\xff")),
            (b"\x01a".to_vec(), b"\x01\xff\xff".to_vec())
        );
        let (start, _) = user_range(None, None);
        assert!(FORMAT_KEY < start.as_slice());
        assert_eq!(strip(b"\x01abc"), b"abc");
    }
}
