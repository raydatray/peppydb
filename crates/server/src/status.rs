//! `engine::Error` to gRPC status (plan 0026, "Error mapping").

use engine::{Error, TreeError};
use tonic::{Code, Status};

/// The gRPC status for an engine error.
pub(crate) fn status(error: &Error) -> Status {
    let code = code(error);
    if code == Code::Internal {
        tracing::error!(%error, "engine error");
    }
    Status::new(code, error.to_string())
}

fn code(error: &Error) -> Code {
    match error {
        Error::Tree(
            TreeError::EmptyKey
            | TreeError::KeyTooLarge
            | TreeError::ValueTooLarge
            | TreeError::InvalidRange,
        ) => Code::InvalidArgument,
        // Sticky: every later write fails until a reopen.
        Error::Wal(_) => Code::Unavailable,
        // Store and I/O failures, and errors that cannot happen after a
        // standalone open (a server bug).
        _ => Code::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_errors_map_to_documented_codes() {
        let cases = [
            (Error::Tree(TreeError::EmptyKey), Code::InvalidArgument),
            (Error::Tree(TreeError::KeyTooLarge), Code::InvalidArgument),
            (Error::Tree(TreeError::ValueTooLarge), Code::InvalidArgument),
            (Error::Tree(TreeError::InvalidRange), Code::InvalidArgument),
            (Error::Wal(engine::WalError::Closed), Code::Unavailable),
            (
                Error::Tree(TreeError::InvalidStore("damaged")),
                Code::Internal,
            ),
            (Error::WrongMode, Code::Internal),
            (Error::Locked, Code::Internal),
            (Error::Options("limits"), Code::Internal),
        ];
        for (error, expected) in cases {
            assert_eq!(status(&error).code(), expected, "{error}");
        }
    }
}
