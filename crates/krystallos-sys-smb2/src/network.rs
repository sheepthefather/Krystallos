//! One-time platform network initialisation.
//!
//! Winsock has to be brought up before any socket call on Windows, and **libsmb2
//! does not do it** — `lib/socket.c:1464` merely reports "Winsock was not
//! initialized. Please call WSAStartup()." and fails. That is the library
//! behaving correctly: process-wide socket initialisation belongs to the
//! application, not to a library that might be one of several users.
//!
//! Doing it once, lazily, and from the FFI layer keeps the platform detail where
//! it belongs. On every other platform this is a no-op.

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;
    use windows_sys::Win32::Networking::WinSock::{WSAStartup, WSADATA};

    /// Winsock 2.2. `MAKEWORD(2, 2)`.
    const VERSION_REQUESTED: u16 = 0x0202;

    /// `None` means it worked. Cached so a failure is reported identically on
    /// every call rather than being retried.
    static OUTCOME: OnceLock<Option<String>> = OnceLock::new();

    pub fn ensure() -> Result<(), String> {
        let outcome = OUTCOME.get_or_init(|| {
            // `WSADATA` is around 400 bytes of fields we never read, and there
            // is no reason to believe a zeroed one is meaningful — only that
            // the call is allowed to write into it. `MaybeUninit` says exactly
            // that, and skips the zeroing.
            let mut data = std::mem::MaybeUninit::<WSADATA>::uninit();
            // SAFETY: `data` is a valid, writable, correctly-sized and aligned
            // `WSADATA`. `WSAStartup` writes into it and does not retain the
            // pointer. The version requested is one Winsock understands, and
            // the call is guarded by `OnceLock` so it happens at most once.
            let rc = unsafe { WSAStartup(VERSION_REQUESTED, data.as_mut_ptr()) };
            if rc == 0 {
                None
            } else {
                Some(format!(
                    "WSAStartup failed with error {rc}; the SMB backend cannot \
                     open sockets on this system"
                ))
            }
        });

        match outcome {
            Some(message) => Err(message.clone()),
            None => Ok(()),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// Nothing to do: POSIX sockets need no process-wide setup.
    pub fn ensure() -> Result<(), String> {
        Ok(())
    }
}

/// Bring up the platform network stack if it needs it. Idempotent and cheap
/// after the first call.
pub fn ensure_network_ready() -> Result<(), String> {
    imp::ensure()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialisation_is_idempotent() {
        // Called on every connect, so it has to be safe to call repeatedly.
        ensure_network_ready().expect("network initialisation should succeed");
        ensure_network_ready().expect("second call should also succeed");
    }
}
