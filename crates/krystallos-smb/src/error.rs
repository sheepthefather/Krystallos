use krystallos_core::Error;
use krystallos_sys_smb2::ffi;
use std::ffi::CStr;

/// Windows NT status codes, as SMB carries them.
///
/// Mapping these rather than raw `errno` is deliberate. `errno` values differ
/// between platforms for exactly the failures that matter most — `ECONNRESET`
/// is 104 on Linux and 100 on MSVC, `ETIMEDOUT` is 110 versus 138 — so an errno
/// table would need a per-platform copy and would be wrong on whichever one
/// nobody tested. NT status codes are part of the protocol and are the same
/// everywhere.
mod status {
    pub const OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
    pub const OBJECT_PATH_NOT_FOUND: u32 = 0xC000_003A;
    pub const NO_SUCH_FILE: u32 = 0xC000_000F;
    pub const BAD_NETWORK_NAME: u32 = 0xC000_00CC;
    pub const ACCESS_DENIED: u32 = 0xC000_0022;
    pub const NETWORK_ACCESS_DENIED: u32 = 0xC000_00CA;
    pub const OBJECT_NAME_COLLISION: u32 = 0xC000_0035;
    pub const NOT_A_DIRECTORY: u32 = 0xC000_0103;
    pub const FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
    pub const DIRECTORY_NOT_EMPTY: u32 = 0xC000_0101;
    pub const CANNOT_DELETE: u32 = 0xC000_0121;

    pub const LOGON_FAILURE: u32 = 0xC000_006D;
    pub const ACCOUNT_RESTRICTION: u32 = 0xC000_006E;
    pub const ACCOUNT_DISABLED: u32 = 0xC000_0072;
    pub const PASSWORD_EXPIRED: u32 = 0xC000_0071;

    pub const NETWORK_NAME_DELETED: u32 = 0xC000_00C9;
    pub const USER_SESSION_DELETED: u32 = 0xC000_0203;
    pub const CONNECTION_DISCONNECTED: u32 = 0xC000_020C;
    pub const CONNECTION_RESET: u32 = 0xC000_020D;
    pub const NETWORK_SESSION_EXPIRED: u32 = 0xC000_035C;

    pub const NOT_SUPPORTED: u32 = 0xC000_00BB;
    pub const INVALID_DEVICE_REQUEST: u32 = 0xC000_0010;
}

/// `errno` values, spelled per platform.
///
/// Most are the same everywhere, but the ones that are not are precisely the
/// ones worth distinguishing: `ENOTEMPTY` is 39 on Linux and 41 on MSVC,
/// `ECONNREFUSED` is 111 and 107. Getting these wrong would silently
/// misclassify on one platform only.
mod errno {
    #[cfg(windows)]
    pub const ENOENT: i32 = 2;
    #[cfg(windows)]
    pub const EACCES: i32 = 13;
    #[cfg(windows)]
    pub const EINVAL: i32 = 22;
    #[cfg(windows)]
    pub const ENOTDIR: i32 = 20;
    #[cfg(windows)]
    pub const EISDIR: i32 = 21;
    #[cfg(windows)]
    pub const ENOTEMPTY: i32 = 41;
    #[cfg(windows)]
    pub const ECONNREFUSED: i32 = 107;

    #[cfg(unix)]
    pub use libc::{EACCES, ECONNREFUSED, EINVAL, EISDIR, ENOENT, ENOTEMPTY, ENOTDIR};
}

/// Build an error describing why a libsmb2 call failed.
///
/// # Why this looks in three places
///
/// libsmb2 does not report failures through one channel, and which channel is
/// populated depends on which code path failed. All three of these were
/// observed against a real server, not guessed at:
///
/// - **`smb2_get_nterror`** is set by `smb2_set_nterror`, which most paths use
///   (`libsmb2.c:3012`, `:3354`, `:3452`, `:3785` and others).
/// - **The message** carries the status name when a path uses
///   `smb2_set_error` instead. The Create path does exactly that
///   (`libsmb2.c:2092`: `"Create failed with status %s."`), which is why a
///   colliding `mkdir` used to arrive as an unclassified error despite the
///   message naming `STATUS_OBJECT_NAME_COLLISION`.
/// - **The return value** is `-errno`, and is the only signal left when the
///   message is empty — which is what a `stat` on a missing file produces.
///
/// Consulting each in turn costs little and means no single upstream quirk
/// leaves the caller unable to tell "no such file" from "server unreachable".
pub(crate) fn from_context(
    ctx: *mut ffi::smb2_context,
    rc: i32,
    path: Option<&str>,
) -> Error {
    // SAFETY: `ctx` is a live context owned by the calling thread, and both
    // accessors are read-only. `smb2_get_error` returns a pointer owned by the
    // context that the next libsmb2 call would invalidate, so it is copied out
    // and not held.
    let message = unsafe {
        let p = ffi::smb2_get_error(ctx);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    let nterror = unsafe { ffi::smb2_get_nterror(ctx) } as u32;

    from_parts(nterror, rc, message, path)
}

fn from_parts(nterror: u32, rc: i32, message: String, path: Option<&str>) -> Error {
    let at = || path.unwrap_or("").to_string();

    if nterror != 0 {
        if let Some(e) = from_status(nterror, &message, path) {
            return e;
        }
    }

    // The message may name a status the numeric channel did not carry. The
    // token is a fixed identifier from libsmb2's own table, not prose, so
    // matching on it is stable.
    if let Some(code) = status_token(&message).and_then(status_from_name) {
        if let Some(e) = from_status(code, &message, path) {
            return e;
        }
    }

    if rc != 0 {
        // libsmb2's sync wrappers return a bare `-1` for a generic failure —
        // a failed `poll`, an expired timeout with no socket, or a
        // `smb2_service` that reported the context unrecoverable
        // (`lib/sync.c:76-95`). All three mean the connection is gone.
        //
        // This must not be fed to the errno table. `-1` is indistinguishable
        // from `-EPERM` there, which is how an unreachable server came to be
        // reported as a permissions problem — the most misleading answer
        // available, since it points at the wrong layer entirely.
        if rc == -1 {
            return Error::ConnectionLost {
                message: if message.is_empty() {
                    "the SMB connection was lost".to_string()
                } else {
                    message
                },
            };
        }

        if let Some(e) = from_errno(-rc, &message, path) {
            return e;
        }
    }

    Error::Backend {
        message: if message.is_empty() {
            // Nothing usable from any channel. Report what there is, without
            // inventing anything: naming a zero status here would print
            // `STATUS_SUCCESS` on a failure path, which reads as exactly the
            // opposite of what happened.
            if nterror != 0 {
                format!(
                    "SMB operation failed with no description ({}, errno {})",
                    status_name(nterror),
                    -rc
                )
            } else {
                format!("SMB operation failed with no description (errno {})", -rc)
            }
        } else {
            message
        },
    }
    .tap_path(at())
}

/// Attach the path to variants that carry one, so the caller always learns
/// which path failed.
trait TapPath {
    fn tap_path(self, path: String) -> Self;
}

impl TapPath for Error {
    fn tap_path(self, path: String) -> Self {
        if path.is_empty() {
            return self;
        }
        match self {
            Error::NotFound { .. } => Error::NotFound { path },
            Error::PermissionDenied { .. } => Error::PermissionDenied { path },
            Error::AlreadyExists { .. } => Error::AlreadyExists { path },
            Error::NotADirectory { .. } => Error::NotADirectory { path },
            Error::IsADirectory { .. } => Error::IsADirectory { path },
            Error::DirectoryNotEmpty { .. } => Error::DirectoryNotEmpty { path },
            other => other,
        }
    }
}

fn from_status(status: u32, message: &str, path: Option<&str>) -> Option<Error> {
    let at = || path.unwrap_or("").to_string();
    Some(match status {
        status::OBJECT_NAME_NOT_FOUND
        | status::OBJECT_PATH_NOT_FOUND
        | status::NO_SUCH_FILE
        // The share itself does not exist, or the DFS target behind it is gone.
        | status::BAD_NETWORK_NAME => Error::NotFound { path: at() },

        status::ACCESS_DENIED | status::NETWORK_ACCESS_DENIED => {
            Error::PermissionDenied { path: at() }
        }

        // `mkdir` over an existing name, and the rename equivalent.
        status::OBJECT_NAME_COLLISION => Error::AlreadyExists { path: at() },

        status::NOT_A_DIRECTORY => Error::NotADirectory { path: at() },

        status::FILE_IS_A_DIRECTORY | status::CANNOT_DELETE => Error::IsADirectory { path: at() },

        status::DIRECTORY_NOT_EMPTY => Error::DirectoryNotEmpty { path: at() },

        status::LOGON_FAILURE
        | status::ACCOUNT_RESTRICTION
        | status::ACCOUNT_DISABLED
        | status::PASSWORD_EXPIRED => Error::Auth {
            message: auth_message(message),
        },

        // These mean the session, or the tree underneath it, is gone, so every
        // later call on the same session fails too. Reporting them as generic
        // errors would leave callers retrying against a corpse.
        status::NETWORK_NAME_DELETED
        | status::USER_SESSION_DELETED
        | status::CONNECTION_DISCONNECTED
        | status::CONNECTION_RESET
        | status::NETWORK_SESSION_EXPIRED => Error::ConnectionLost {
            message: message.to_string(),
        },

        status::NOT_SUPPORTED | status::INVALID_DEVICE_REQUEST => Error::Unsupported {
            operation: "this operation on this server",
        },

        _ => return None,
    })
}

/// Frame libsmb2's text for an authentication failure.
///
/// The status channel is definitive here — the server rejected the credentials
/// — but libsmb2's own description is often a *later* symptom of the rejected
/// session, typically the socket being torn down. Presented bare, "Read from
/// socket failed" sends whoever is reading it to debug their network instead of
/// their password. So the detail is kept, but marked as secondary.
fn auth_message(libsmb2_text: &str) -> String {
    let detail = libsmb2_text.trim();
    if detail.is_empty() {
        "the server rejected the credentials".to_string()
    } else {
        format!("the server rejected the credentials (libsmb2 reported: {detail})")
    }
}

fn from_errno(errno: i32, message: &str, path: Option<&str>) -> Option<Error> {
    let at = || path.unwrap_or("").to_string();
    Some(match errno {
        errno::ENOENT => Error::NotFound { path: at() },
        // `EPERM` is deliberately absent. libsmb2 maps several unrelated
        // statuses onto it — `STATUS_FILE_IS_A_DIRECTORY` and
        // `STATUS_CANNOT_DELETE` among them (`lib/errors.c:1123-1128`) — so it
        // cannot be read as "permission denied" without guessing. EACCES is the
        // unambiguous permission signal, and an unmapped EPERM falls through to
        // the message, which says what actually happened.
        errno::EACCES => Error::PermissionDenied { path: at() },
        errno::ENOTDIR => Error::NotADirectory { path: at() },
        errno::EISDIR => Error::IsADirectory { path: at() },
        errno::ENOTEMPTY => Error::DirectoryNotEmpty { path: at() },
        // libsmb2 maps STATUS_LOGON_FAILURE here (`lib/errors.c:1133`).
        errno::ECONNREFUSED => Error::Auth {
            message: auth_message(message),
        },
        errno::EINVAL => Error::Unsupported {
            operation: "this operation on this server",
        },
        _ => return None,
    })
}

/// Pull a `STATUS_SOMETHING` identifier out of an error message.
fn status_token(message: &str) -> Option<&str> {
    let start = message.find("STATUS_")?;
    let rest = &message[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Names libsmb2 puts into messages, mapped back to codes.
///
/// Only the ones worth classifying are listed; the full table is libsmb2's
/// `lib/errors.c`. An unrecognised name simply falls through to the next
/// channel.
fn status_from_name(name: &str) -> Option<u32> {
    Some(match name {
        "STATUS_OBJECT_NAME_NOT_FOUND" => status::OBJECT_NAME_NOT_FOUND,
        "STATUS_OBJECT_PATH_NOT_FOUND" => status::OBJECT_PATH_NOT_FOUND,
        "STATUS_NO_SUCH_FILE" => status::NO_SUCH_FILE,
        "STATUS_BAD_NETWORK_NAME" => status::BAD_NETWORK_NAME,
        "STATUS_ACCESS_DENIED" => status::ACCESS_DENIED,
        "STATUS_NETWORK_ACCESS_DENIED" => status::NETWORK_ACCESS_DENIED,
        "STATUS_OBJECT_NAME_COLLISION" => status::OBJECT_NAME_COLLISION,
        "STATUS_NOT_A_DIRECTORY" => status::NOT_A_DIRECTORY,
        "STATUS_FILE_IS_A_DIRECTORY" => status::FILE_IS_A_DIRECTORY,
        "STATUS_DIRECTORY_NOT_EMPTY" => status::DIRECTORY_NOT_EMPTY,
        "STATUS_CANNOT_DELETE" => status::CANNOT_DELETE,
        "STATUS_LOGON_FAILURE" => status::LOGON_FAILURE,
        "STATUS_ACCOUNT_RESTRICTION" => status::ACCOUNT_RESTRICTION,
        "STATUS_ACCOUNT_DISABLED" => status::ACCOUNT_DISABLED,
        "STATUS_PASSWORD_EXPIRED" => status::PASSWORD_EXPIRED,
        "STATUS_NETWORK_NAME_DELETED" => status::NETWORK_NAME_DELETED,
        "STATUS_USER_SESSION_DELETED" => status::USER_SESSION_DELETED,
        "STATUS_CONNECTION_DISCONNECTED" => status::CONNECTION_DISCONNECTED,
        "STATUS_CONNECTION_RESET" => status::CONNECTION_RESET,
        "STATUS_NETWORK_SESSION_EXPIRED" => status::NETWORK_SESSION_EXPIRED,
        "STATUS_NOT_SUPPORTED" => status::NOT_SUPPORTED,
        _ => return None,
    })
}

/// Render an NT status the way libsmb2's own table does, for diagnostics.
pub fn status_name(status: u32) -> String {
    // SAFETY: total over u32; returns a pointer to static storage.
    unsafe {
        let p = ffi::nterror_to_str(status);
        if p.is_null() {
            return format!("{status:#010X}");
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three channels are exercised separately, because each one was the
    /// only populated channel in a real failure we hit.
    fn by_status(status: u32) -> Error {
        from_parts(status, 0, String::new(), Some("/a"))
    }
    fn by_message(message: &str) -> Error {
        from_parts(0, 0, message.to_string(), Some("/a"))
    }
    fn by_errno(errno: i32) -> Error {
        from_parts(0, -errno, String::new(), Some("/a"))
    }

    #[test]
    fn not_found_variants_all_map_to_not_found() {
        for status in [
            status::OBJECT_NAME_NOT_FOUND,
            status::OBJECT_PATH_NOT_FOUND,
            status::NO_SUCH_FILE,
            status::BAD_NETWORK_NAME,
        ] {
            match by_status(status) {
                Error::NotFound { path } => assert_eq!(path, "/a"),
                other => panic!("status {status:#010X} mapped to {other:?}"),
            }
        }
    }

    #[test]
    fn the_create_path_is_classified_from_its_message() {
        // The regression this guards: `mkdir` over an existing name produced
        // "Create failed with status STATUS_OBJECT_NAME_COLLISION." with
        // `nterror` left at zero, so it arrived as an unclassified backend
        // error. libsmb2.c:2092 uses `smb2_set_error` there rather than
        // `smb2_set_nterror`.
        match by_message("Create failed with status STATUS_OBJECT_NAME_COLLISION.") {
            Error::AlreadyExists { path } => assert_eq!(path, "/a"),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn a_silent_failure_is_classified_from_the_errno() {
        // `stat` on a missing file produced an empty message and a zero status;
        // the return value was the only signal.
        match by_errno(errno::ENOENT) {
            Error::NotFound { path } => assert_eq!(path, "/a"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn authentication_failures_are_distinct_from_permission_denied() {
        // Different things to a user: "wrong password, fix your settings"
        // versus "this account cannot read that file".
        assert!(matches!(
            by_status(status::LOGON_FAILURE),
            Error::Auth { .. }
        ));
        assert!(matches!(
            by_errno(errno::ECONNREFUSED),
            Error::Auth { .. }
        ));

        let perm = by_status(status::ACCESS_DENIED);
        assert!(matches!(perm, Error::PermissionDenied { .. }));
        assert!(!perm.is_fatal_to_session());
    }

    #[test]
    fn session_level_failures_are_marked_fatal() {
        for status in [
            status::NETWORK_NAME_DELETED,
            status::USER_SESSION_DELETED,
            status::CONNECTION_DISCONNECTED,
            status::CONNECTION_RESET,
            status::NETWORK_SESSION_EXPIRED,
        ] {
            let e = by_status(status);
            assert!(
                e.is_fatal_to_session(),
                "status {status:#010X} should be fatal to the session, got {e:?}"
            );
        }
    }

    #[test]
    fn an_unknown_status_keeps_the_original_text() {
        // Losing libsmb2's own description would make server-specific quirks
        // undiagnosable.
        match from_parts(0xDEAD_BEEF, 0, "server said something odd".into(), None) {
            Error::Backend { message } => assert_eq!(message, "server said something odd"),
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_with_nothing_to_say_still_says_something() {
        match from_parts(0, -22, String::new(), None) {
            Error::Unsupported { .. } => {}
            other => panic!("errno EINVAL should be Unsupported, got {other:?}"),
        }
        // Genuinely unknown: the message must still be printable and must not
        // claim success.
        match from_parts(0, -9999, String::new(), None) {
            Error::Backend { message } => {
                assert!(!message.is_empty());
                assert!(!message.contains("STATUS_SUCCESS"), "got: {message}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn status_token_extraction_stops_at_the_identifier() {
        assert_eq!(
            status_token("Create failed with status STATUS_OBJECT_NAME_COLLISION."),
            Some("STATUS_OBJECT_NAME_COLLISION")
        );
        assert_eq!(status_token("read from socket failed, errno:10038"), None);
        assert_eq!(
            status_token("STATUS_ACCESS_DENIED"),
            Some("STATUS_ACCESS_DENIED")
        );
    }

    #[test]
    fn a_stale_nterror_does_not_override_a_clearer_message() {
        // `nterror` is not reset on every path, so it can hold a value from an
        // earlier operation. An unmapped stale value must fall through to the
        // other channels rather than being reported as-is.
        match from_parts(0x0000_0001, 0, "Create failed with status STATUS_ACCESS_DENIED.".into(), None)
        {
            Error::PermissionDenied { .. } => {}
            other => panic!("expected the message to win, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_minus_one_is_a_lost_connection_not_a_permission_problem() {
        // The regression this guards: an unreachable server (a connect that
        // times out with no socket) reported `permission denied`, because
        // libsmb2's generic `-1` was read as `-EPERM`. That points the reader
        // at the wrong layer entirely.
        let e = from_parts(0, -1, "smb2_service: POLLHUP, socket error.".into(), Some("/a"));
        assert!(
            matches!(e, Error::ConnectionLost { .. }),
            "a bare -1 should be a lost connection, got {e:?}"
        );
        assert!(e.is_fatal_to_session());
    }

    #[test]
    fn a_genuine_errno_beyond_minus_one_still_classifies() {
        // Only `-1` is the generic failure marker; real errno values must keep
        // working.
        assert!(matches!(
            from_parts(0, -(errno::ENOENT), String::new(), None),
            Error::NotFound { .. }
        ));
    }

    #[test]
    fn the_status_channel_outranks_a_generic_failure_code() {
        // A rejected logon tears the socket down, so the call also returns
        // `-1`. The status is the real answer and must win.
        let e = from_parts(
            status::LOGON_FAILURE,
            -1,
            "smb2_service failed with : Read from socket failed, errno:10038.".into(),
            None,
        );
        assert!(matches!(e, Error::Auth { .. }), "got {e:?}");
    }

    #[test]
    fn an_auth_failure_does_not_read_as_a_network_problem() {
        // libsmb2's own text after a rejected logon is a socket symptom. Shown
        // bare it sends people to debug their network instead of their password.
        match by_status(status::LOGON_FAILURE) {
            Error::Auth { message } => {
                assert!(
                    message.contains("credentials"),
                    "should say what actually went wrong: {message}"
                );
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn status_names_come_from_libsmb2() {
        assert_eq!(
            status_name(status::OBJECT_NAME_NOT_FOUND),
            "STATUS_OBJECT_NAME_NOT_FOUND"
        );
        assert!(!status_name(0xDEAD_BEEF).is_empty());
    }
}
