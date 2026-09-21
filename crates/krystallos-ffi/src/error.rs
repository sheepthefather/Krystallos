//! Errors crossing the FFI boundary.
//!
//! # Why this is a separate type rather than `krystallos_core::Error`
//!
//! `krystallos_core::Error` is the kernel's internal error model, and it should
//! stay free to change as the kernel does. Everything exported through UniFFI
//! becomes part of a published API — Kotlin code that a caller has already
//! written against it. Binding the two directly would mean any internal
//! refactor is a breaking change for Android.
//!
//! So the boundary gets its own type, and the mapping is explicit and testable.
//! It also lets the FFI error carry only what a caller can act on: the internal
//! error's `Io(std::io::Error)` variant has no meaning on the Kotlin side, and
//! `ConnectionLost` versus `NotFound` is the distinction a UI actually needs —
//! one means "offer to reconnect", the other means "that file is gone".

use krystallos_core::Error as CoreError;

/// A failure the Android side can act on.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum KernelError {
    /// The path does not exist.
    #[error("not found: {path}")]
    NotFound { path: String },

    /// The server refused access.
    #[error("permission denied: {path}")]
    PermissionDenied { path: String },

    /// Something is already there — a `mkdir` over an existing directory, or a
    /// create that was asked not to clobber.
    #[error("already exists: {path}")]
    AlreadyExists { path: String },

    #[error("not a directory: {path}")]
    NotADirectory { path: String },

    #[error("is a directory: {path}")]
    IsADirectory { path: String },

    #[error("directory not empty: {path}")]
    DirectoryNotEmpty { path: String },

    /// The path cannot be represented on this backend at all — a character the
    /// protocol forbids, or a name longer than the server accepts.
    #[error("path not usable on this backend: {path} ({reason})")]
    InvalidPath { path: String, reason: String },

    /// The backend cannot do this. Check `Capabilities` before relying on an
    /// operation you need to degrade gracefully.
    #[error("not supported: {operation}")]
    Unsupported { operation: String },

    /// The session is gone; every later call on it will fail too. **Reconnect
    /// rather than retry.**
    ///
    /// The field is `detail`, not `message`, on purpose: UniFFI generates an
    /// `override val message` for every error variant, so a field named
    /// `message` collides with it and the generated Kotlin fails to compile
    /// with `REDECLARATION`.
    #[error("connection lost: {detail}")]
    ConnectionLost { detail: String },

    /// Credentials were rejected. Distinct from `PermissionDenied`: this one
    /// means "fix your settings", that one means "this account cannot read that
    /// file".
    #[error("authentication failed: {detail}")]
    Auth { detail: String },

    /// Anything else, with the backend's own description preserved. Losing that
    /// text would make server-specific quirks undiagnosable.
    #[error("backend error: {detail}")]
    Backend { detail: String },
}

impl From<CoreError> for KernelError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::NotFound { path } => KernelError::NotFound { path },
            CoreError::PermissionDenied { path } => KernelError::PermissionDenied { path },
            CoreError::AlreadyExists { path } => KernelError::AlreadyExists { path },
            CoreError::NotADirectory { path } => KernelError::NotADirectory { path },
            CoreError::IsADirectory { path } => KernelError::IsADirectory { path },
            CoreError::DirectoryNotEmpty { path } => KernelError::DirectoryNotEmpty { path },
            CoreError::InvalidPath { path, reason } => KernelError::InvalidPath { path, reason },
            CoreError::Unsupported { operation } => KernelError::Unsupported {
                operation: operation.to_string(),
            },
            CoreError::ConnectionLost { message } => KernelError::ConnectionLost { detail: message },
            CoreError::Auth { message } => KernelError::Auth { detail: message },
            CoreError::Backend { message } => KernelError::Backend { detail: message },
            // `std::io::Error` has no portable equivalent on the Kotlin side,
            // and its `Display` is the only part a caller could use. Reporting
            // it as a backend error keeps the message without inventing a
            // category Android cannot act on differently anyway.
            CoreError::Io(e) => KernelError::Backend {
                detail: e.to_string(),
            },
        }
    }
}

/// `Result` as the Kotlin bindings see it.
pub type Result<T, E = KernelError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every core variant must map to a distinct FFI variant. A collapse would
    /// silently cost the caller the ability to tell "gone" from "forbidden".
    #[test]
    fn each_core_variant_maps_to_its_own_ffi_variant() {
        let cases: Vec<(CoreError, &str)> = vec![
            (CoreError::not_found("/a"), "NotFound"),
            (
                CoreError::PermissionDenied {
                    path: "/a".into(),
                },
                "PermissionDenied",
            ),
            (
                CoreError::AlreadyExists {
                    path: "/a".into(),
                },
                "AlreadyExists",
            ),
            (
                CoreError::NotADirectory {
                    path: "/a".into(),
                },
                "NotADirectory",
            ),
            (
                CoreError::IsADirectory {
                    path: "/a".into(),
                },
                "IsADirectory",
            ),
            (
                CoreError::DirectoryNotEmpty {
                    path: "/a".into(),
                },
                "DirectoryNotEmpty",
            ),
            (
                CoreError::InvalidPath {
                    path: "/a".into(),
                    reason: "why".into(),
                },
                "InvalidPath",
            ),
            (
                CoreError::Unsupported {
                    operation: "op",
                },
                "Unsupported",
            ),
            (CoreError::connection_lost("gone"), "ConnectionLost"),
            (
                CoreError::Auth {
                    message: "bad password".into(),
                },
                "Auth",
            ),
            (CoreError::backend("odd"), "Backend"),
        ];

        for (core, expected) in cases {
            let mapped: KernelError = core.into();
            let actual = match mapped {
                KernelError::NotFound { .. } => "NotFound",
                KernelError::PermissionDenied { .. } => "PermissionDenied",
                KernelError::AlreadyExists { .. } => "AlreadyExists",
                KernelError::NotADirectory { .. } => "NotADirectory",
                KernelError::IsADirectory { .. } => "IsADirectory",
                KernelError::DirectoryNotEmpty { .. } => "DirectoryNotEmpty",
                KernelError::InvalidPath { .. } => "InvalidPath",
                KernelError::Unsupported { .. } => "Unsupported",
                KernelError::ConnectionLost { .. } => "ConnectionLost",
                KernelError::Auth { .. } => "Auth",
                KernelError::Backend { .. } => "Backend",
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn an_io_error_keeps_its_message_rather_than_being_flattened() {
        let e: KernelError = CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the socket gave up",
        ))
        .into();
        match e {
            KernelError::Backend { detail: message } => {
                assert!(message.contains("the socket gave up"), "got: {message}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn session_level_failures_stay_distinguishable_across_the_boundary() {
        // This is the distinction the Android side branches on to decide
        // whether to offer a reconnect, so it must survive the mapping.
        let lost: KernelError = CoreError::connection_lost("gone").into();
        assert!(matches!(lost, KernelError::ConnectionLost { .. }));

        let auth: KernelError = CoreError::Auth {
            message: "bad".into(),
        }
        .into();
        assert!(matches!(auth, KernelError::Auth { .. }));

        let missing: KernelError = CoreError::not_found("/a").into();
        assert!(matches!(missing, KernelError::NotFound { .. }));
    }

    #[test]
    fn error_messages_are_human_readable() {
        // These strings surface in the app's UI, so they are not just for logs.
        let e: KernelError = CoreError::not_found("/movies/a.mkv").into();
        assert_eq!(e.to_string(), "not found: /movies/a.mkv");
    }
}
