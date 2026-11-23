//! The stored key layout (plan 0026, "Key layout"), from `server-keys`
//! (plan 0030), with the tonic error mapping and the format check.

use engine::Db;
use tonic::Status;

pub(crate) use server_keys::{FORMAT_KEY, FORMAT_VERSION, strip, user_range};

/// The stored key for a user key: `0x01 ‖ key`. An empty user key is
/// `INVALID_ARGUMENT`, as in the engine.
pub(crate) fn user_key(key: &[u8]) -> Result<Vec<u8>, Status> {
    server_keys::user_key(key).map_err(|_| Status::invalid_argument("key must not be empty"))
}

/// Validate stored key and optional value against the engine's tree limits
/// before proposing to Raft (plan 0030 discovery: an entry the engine refuses
/// halts the state machine).
pub(crate) fn check_limits(
    stored_key: &[u8],
    value: Option<&[u8]>,
    max_key_len: usize,
    max_value_len: usize,
) -> Result<(), Status> {
    if stored_key.len() > max_key_len {
        return Err(Status::invalid_argument("key exceeds server limit"));
    }
    if let Some(v) = value
        && v.len() > max_value_len
    {
        return Err(Status::invalid_argument("value exceeds server limit"));
    }
    Ok(())
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

/// Check the format marker in a `Mode::External` database without writing.
/// Returns `Ok(true)` if the marker is present and matches, or `Ok(false)` if
/// the database is empty (or holds only Raft system keys before the marker is
/// proposed).
pub(crate) fn check_format_readonly(db: &Db) -> Result<bool, FormatError> {
    match db.get(FORMAT_KEY)? {
        Some(version) if version == FORMAT_VERSION => Ok(true),
        Some(version) => Err(FormatError::Unsupported(version)),
        None => {
            if db.range(None, None)?.next().is_some() && db.get(server_keys::APPLIED_KEY)?.is_none()
            {
                return Err(FormatError::Missing);
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_user_key_is_invalid_argument() {
        assert_eq!(user_key(b"k").unwrap(), b"\x01k");
        assert_eq!(
            user_key(b"").unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }
}
