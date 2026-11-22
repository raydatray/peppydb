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
    fn an_empty_user_key_is_invalid_argument() {
        assert_eq!(user_key(b"k").unwrap(), b"\x01k");
        assert_eq!(
            user_key(b"").unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }
}
