use std::fmt;

/// Errors surfaced by a storage backend.
///
/// The variants are chosen so callers can make decisions, not merely display
/// text. In particular [`Error::ConnectionLost`] is kept apart from
/// [`Error::Io`]: the first means the session is gone and every subsequent
/// operation on this backend will also fail, while the second is a per-operation
/// failure that leaves the session usable.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {path}")]
    NotFound { path: String },

    #[error("permission denied: {path}")]
    PermissionDenied { path: String },

    #[error("already exists: {path}")]
    AlreadyExists { path: String },

    #[error("not a directory: {path}")]
    NotADirectory { path: String },

    #[error("is a directory: {path}")]
    IsADirectory { path: String },

    #[error("directory not empty: {path}")]
    DirectoryNotEmpty { path: String },

    /// The path could not be represented on the target backend — for example it
    /// contains a character the protocol forbids, or it is longer than the
    /// server accepts. This is not a caller bug in the way an out-of-range
    /// index is; it is a genuine capability limit of the storage.
    #[error("path not representable on this backend: {path} ({reason})")]
    InvalidPath { path: String, reason: String },

    /// The backend cannot perform this operation at all. Check
    /// [`Capabilities`](crate::Capabilities) before relying on an operation if
    /// you need to handle this gracefully.
    #[error("operation not supported by this backend: {operation}")]
    Unsupported { operation: &'static str },

    /// The underlying session is broken. Reconnecting is required.
    #[error("connection lost: {message}")]
    ConnectionLost { message: String },

    #[error("authentication failed: {message}")]
    Auth { message: String },

    /// The server rejected the request for a reason that has no portable
    /// equivalent. Carries the backend's own description so it is not lost.
    #[error("backend error: {message}")]
    Backend { message: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Whether retrying the *same operation* on the same session could succeed.
    ///
    /// [`Error::ConnectionLost`] is deliberately `false`: retrying against a
    /// dead session just fails again. Callers should re-establish the session
    /// first, then retry.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Io(e) if e.kind() == std::io::ErrorKind::Interrupted)
    }

    /// Whether this error means the session must be re-established.
    pub fn is_fatal_to_session(&self) -> bool {
        matches!(self, Error::ConnectionLost { .. } | Error::Auth { .. })
    }

    /// Shorthand for the most common variant, for backend implementations.
    pub fn not_found(path: impl fmt::Display) -> Self {
        Error::NotFound {
            path: path.to_string(),
        }
    }

    /// Shorthand used by backends when the transport dies underneath them.
    pub fn connection_lost(message: impl Into<String>) -> Self {
        Error::ConnectionLost {
            message: message.into(),
        }
    }

    /// Wrap a backend-specific failure that has no portable equivalent. Keep
    /// the backend's own description in `message` so it is not lost to the
    /// caller — a bare "backend error" is useless when diagnosing a server
    /// quirk.
    pub fn backend(message: impl Into<String>) -> Self {
        Error::Backend {
            message: message.into(),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_lost_is_not_retryable_but_is_session_fatal() {
        let e = Error::connection_lost("server went away");
        assert!(!e.is_retryable());
        assert!(e.is_fatal_to_session());
    }

    #[test]
    fn not_found_is_neither_retryable_nor_fatal() {
        let e = Error::not_found("/a/b");
        assert!(!e.is_retryable());
        assert!(!e.is_fatal_to_session());
        assert_eq!(e.to_string(), "not found: /a/b");
    }

    #[test]
    fn interrupted_io_is_retryable() {
        let e = Error::Io(std::io::Error::from(std::io::ErrorKind::Interrupted));
        assert!(e.is_retryable());
        assert!(!e.is_fatal_to_session());
    }
}
