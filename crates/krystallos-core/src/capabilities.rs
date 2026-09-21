/// What a backend can actually do.
///
/// Protocols are not interchangeable, and pretending otherwise pushes the
/// discovery to runtime: a caller finds out that positioned writes are
/// impossible only when an upload fails halfway. Declaring capabilities up
/// front lets a caller choose a different strategy instead.
///
/// The concrete case that motivates this: FTP has no positioned read. Random
/// access on FTP means re-issuing `REST` before every `RETR`, which is correct
/// but ruinously slow. A caller that knows this can fall back to sequential
/// reads with a local buffer rather than issuing thousands of round-trips.
///
/// A capability being `false` does not mean the operation is impossible — it
/// means it is not cheap or not native. Backends must not claim a capability
/// they cannot honour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capabilities {
    /// Positioned reads ([`FileHandle::read_at`](crate::FileHandle::read_at))
    /// are native and cheap. `false` means the backend can only read
    /// sequentially, and positioned reads are emulated at a cost.
    pub random_read: bool,

    /// Positioned writes are native. `false` for protocols where writing at an
    /// offset means rewriting the file — WebDAV's non-standard partial `PUT`,
    /// for instance.
    pub random_write: bool,

    /// [`StorageBackend::rename`](crate::StorageBackend::rename) is atomic on
    /// the server.
    pub atomic_rename: bool,

    /// [`FileHandle::set_len`](crate::FileHandle::set_len) is supported.
    pub set_len: bool,

    /// [`FileHandle::flush`](crate::FileHandle::flush) does something beyond a
    /// no-op — that is, it asks the server to make writes durable rather than
    /// merely pushing them out of a local buffer.
    pub durable_flush: bool,

    /// The backend reports modification times.
    pub modification_times: bool,
}

impl Capabilities {
    /// Everything supported, for backends where that is true — a local
    /// filesystem, and SMB.
    pub const FULL: Capabilities = Capabilities {
        random_read: true,
        random_write: true,
        atomic_rename: true,
        set_len: true,
        durable_flush: true,
        modification_times: true,
    };

    /// A conservative baseline for a backend that has not declared anything.
    pub const MINIMAL: Capabilities = Capabilities {
        random_read: true,
        random_write: true,
        atomic_rename: false,
        set_len: true,
        durable_flush: false,
        modification_times: false,
    };
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities::MINIMAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_declares_everything() {
        let c = Capabilities::FULL;
        assert!(c.random_read && c.random_write && c.atomic_rename);
        assert!(c.set_len && c.durable_flush && c.modification_times);
    }

    #[test]
    fn minimal_is_conservative_about_atomicity_and_durability() {
        let c = Capabilities::MINIMAL;
        assert!(!c.atomic_rename, "must not promise atomicity it lacks");
        assert!(!c.durable_flush, "must not promise durability it lacks");
        assert!(!c.modification_times);
    }

    #[test]
    fn default_is_minimal_not_full() {
        // A backend author who forgets to override `capabilities()` should
        // under-promise rather than over-promise.
        assert_eq!(Capabilities::default(), Capabilities::MINIMAL);
    }
}
