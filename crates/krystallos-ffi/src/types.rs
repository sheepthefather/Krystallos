//! The types the Android side sees.
//!
//! These are plain data: no handles, no lifetimes, nothing that would require
//! the caller to reason about Rust's ownership. That is not decoration — UniFFI
//! turns each of these into a Kotlin class, and anything with a lifetime in it
//! would either be rejected outright or produce bindings that are awkward to
//! use from a UI layer.

use crate::error::KernelError;
use krystallos_core::{Entry, EntryKind, Metadata, OpenMode};

/// What kind of thing an entry is.
///
/// A flat enum rather than a set of flags, matching the kernel's model: the
/// finer distinctions protocols disagree about (SMB reparse points, POSIX mode
/// bits) stay inside the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Kind {
    File,
    Directory,
    /// A symlink or protocol equivalent. Whether the backend follows it when
    /// resolving a path is backend-specific.
    Symlink,
    /// Neither file nor directory and no portable equivalent — an SMB named
    /// pipe, a device node.
    Other,
}

impl From<EntryKind> for Kind {
    fn from(k: EntryKind) -> Self {
        match k {
            EntryKind::File => Kind::File,
            EntryKind::Directory => Kind::Directory,
            EntryKind::Symlink => Kind::Symlink,
            EntryKind::Other => Kind::Other,
        }
    }
}

/// Portable metadata for one entry.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EntryMetadata {
    pub kind: Kind,
    pub len: u64,
    /// Milliseconds since the Unix epoch, or `None` when the backend does not
    /// report it.
    ///
    /// `SystemTime` does not cross the FFI boundary, and milliseconds as an
    /// integer is the form Kotlin can turn into whatever it wants. The
    /// alternative — a formatted string — would throw away the ability to sort
    /// or compare.
    pub modified_ms: Option<u64>,
    pub created_ms: Option<u64>,
    pub accessed_ms: Option<u64>,
    pub read_only: bool,
}

impl EntryMetadata {
    pub(crate) fn from_core(m: &Metadata) -> Self {
        EntryMetadata {
            kind: m.kind.into(),
            len: m.len,
            modified_ms: to_millis(m.modified),
            created_ms: to_millis(m.created),
            accessed_ms: to_millis(m.accessed),
            read_only: m.read_only,
        }
    }
}

/// Convert a timestamp to milliseconds since the epoch.
///
/// A time before 1970, or one so far in the future that it overflows, becomes
/// `None` rather than a nonsense number. SMB servers have been known to report
/// both, and a caller sorting by this should not be handed a value that is
/// quietly wrong.
fn to_millis(t: Option<std::time::SystemTime>) -> Option<u64> {
    let d = t?.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(d.as_millis()).ok()
}

/// A directory listing entry.
///
/// The metadata travels with the name on purpose. Splitting them would invite
/// callers to list and then stat each result, which over a network is one
/// round-trip per entry — the fastest way to make a file browser feel broken on
/// a large directory.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DirEntry {
    /// The name **exactly as the backend reported it**.
    ///
    /// Callers must use this verbatim when addressing the entry. Rebuilding it
    /// from user input risks a mismatch on servers that store names in a
    /// different Unicode normalization form, which macOS shares do.
    pub name: String,
    pub metadata: EntryMetadata,
}

impl From<Entry> for DirEntry {
    fn from(e: Entry) -> Self {
        DirEntry {
            name: e.name,
            metadata: EntryMetadata::from_core(&e.metadata),
        }
    }
}

/// How to open a file.
///
/// A record rather than a bitmask so that impossible combinations cannot be
/// expressed, and so Kotlin gets named fields instead of magic numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    /// Create the file if it does not exist.
    pub create: bool,
    /// Fail if it already exists. Turns a check-then-write race into a single
    /// call the server arbitrates.
    pub create_new: bool,
    /// Discard existing contents on open.
    pub truncate: bool,
}

impl OpenFlags {
    /// Open an existing file for reading.
    pub fn read_only() -> Self {
        OpenFlags {
            read: true,
            write: false,
            create: false,
            create_new: false,
            truncate: false,
        }
    }

    /// Open for writing, creating if absent, without truncating.
    ///
    /// Not truncating is the point: a positioned writer has to be able to fill
    /// a file out of order.
    pub fn write_only() -> Self {
        OpenFlags {
            read: false,
            write: true,
            create: true,
            create_new: false,
            truncate: false,
        }
    }

    /// Read and write, creating if absent.
    pub fn read_write() -> Self {
        OpenFlags {
            read: true,
            write: true,
            create: true,
            create_new: false,
            truncate: false,
        }
    }

    /// Create a new file, failing if it exists.
    pub fn create_exclusive() -> Self {
        OpenFlags {
            read: false,
            write: true,
            create: true,
            create_new: true,
            truncate: false,
        }
    }

    /// Same access mode, discarding existing contents on open.
    pub fn with_truncate(mut self) -> Self {
        self.truncate = true;
        self
    }
}

impl From<OpenFlags> for OpenMode {
    fn from(f: OpenFlags) -> Self {
        // Built from the access mode outwards so the result always satisfies
        // `OpenMode`'s invariants, whatever combination arrived.
        let mut mode = match (f.read, f.write) {
            (true, true) => OpenMode::read_write(),
            (false, true) => OpenMode::write(),
            // Read-only covers both "asked for read" and the degenerate
            // "asked for nothing", which is the safe reading of an input the
            // Kotlin side should not be able to produce anyway.
            (true, false) | (false, false) => OpenMode::read(),
        };

        if f.create_new {
            mode = OpenMode::create_new();
        }
        if f.truncate {
            mode = mode.with_truncate();
        }
        mode
    }
}

/// What a backend can do.
///
/// Protocols are not interchangeable, and this is where that becomes visible to
/// the caller instead of being discovered by having an operation fail halfway.
/// FTP, for instance, has no positioned read at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Capabilities {
    pub random_read: bool,
    pub random_write: bool,
    pub atomic_rename: bool,
    pub set_len: bool,
    pub durable_flush: bool,
    pub modification_times: bool,
}

impl From<krystallos_core::Capabilities> for Capabilities {
    fn from(c: krystallos_core::Capabilities) -> Self {
        Capabilities {
            random_read: c.random_read,
            random_write: c.random_write,
            atomic_rename: c.atomic_rename,
            set_len: c.set_len,
            durable_flush: c.durable_flush,
            modification_times: c.modification_times,
        }
    }
}

/// Everything needed to open a session.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ConnectRequest {
    /// A URI such as `smb://host/share` or `file:///D:/media`.
    pub uri: String,
    pub username: Option<String>,
    /// Kept out of the URI on purpose: a password embedded in a URL leaks into
    /// logs, error messages and crash reports.
    pub password: Option<String>,
    /// SMB domain or workgroup. Ignored by backends without the concept.
    pub domain: Option<String>,
    /// Request SMB3 transport encryption.
    ///
    /// Off by default, and that default is measured rather than arbitrary:
    /// libsmb2 uses its own portable reference AES on every platform except
    /// Apple, including Android, and enabling encryption dropped a read from
    /// about 280 MiB/s to under 3. Turn it on when the network is untrusted and
    /// the slower transfer is the right trade.
    pub smb_seal: bool,
}

impl ConnectRequest {
    /// A request with no credentials and encryption off.
    pub fn new(uri: impl Into<String>) -> Self {
        ConnectRequest {
            uri: uri.into(),
            username: None,
            password: None,
            domain: None,
            smb_seal: false,
        }
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    pub fn with_smb_seal(mut self, seal: bool) -> Self {
        self.smb_seal = seal;
        self
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        String,
        krystallos_core::Credentials,
        krystallos_core::ConnectionOptions,
    ) {
        let credentials = krystallos_core::Credentials {
            username: self.username,
            password: self.password,
            domain: self.domain,
        };
        let mut options = krystallos_core::ConnectionOptions::new();
        if self.smb_seal {
            options.set(krystallos_smb::OPT_SEAL, "true");
        }
        (self.uri, credentials, options)
    }
}

/// A byte range read from a file.
///
/// # Why a record rather than a bare `Vec<u8>`
///
/// UniFFI's zero-copy byte support is one-directional — `&[u8]` flows from the
/// foreign side into Rust, and there is no `&mut [u8]` counterpart to write
/// into a caller's buffer. So a read has to return owned bytes, which means a
/// copy at the boundary.
///
/// At the sizes involved that copy is not worth engineering around. A 100 Mbps
/// 4K stream is roughly 12 MiB/s, so with one-mebibyte reads it is about twelve
/// copies a second. The network round-trip those reads sit behind costs
/// milliseconds; the copy costs microseconds.
///
/// Returning a record rather than a bare `Vec<u8>` also leaves room to add a
/// field later — the actual offset served, say — without changing the signature.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ByteRange {
    pub offset: u64,
    /// Fewer bytes than requested means end-of-file was reached.
    pub data: Vec<u8>,
}

impl ByteRange {
    pub(crate) fn new(offset: u64, data: Vec<u8>) -> Self {
        ByteRange { offset, data }
    }

    /// Whether this read reached the end of the file.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.data.len() as u64
    }
}

/// Reject a call on a session or file that has already been closed.
///
/// Reported as `ConnectionLost` rather than a generic error because that is
/// what it is from the caller's side: the handle is gone and no retry on it
/// will work.
pub(crate) fn already_closed(what: &str) -> KernelError {
    KernelError::ConnectionLost {
        detail: format!("this {what} has already been closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn entry_metadata_converts_every_field() {
        let mut m = Metadata::file(4096);
        m.modified = Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123));
        m.read_only = true;

        let ffi = EntryMetadata::from_core(&m);
        assert_eq!(ffi.kind, Kind::File);
        assert_eq!(ffi.len, 4096);
        assert_eq!(ffi.modified_ms, Some(1_700_000_000_123));
        assert!(ffi.read_only);
        // Not reported by the constructor, so it must stay absent rather than
        // becoming a zero that looks like 1970.
        assert_eq!(ffi.created_ms, None);
    }

    #[test]
    fn an_absent_timestamp_stays_absent() {
        assert_eq!(to_millis(None), None);
    }

    #[test]
    fn a_pre_epoch_timestamp_is_reported_as_absent() {
        // `duration_since` fails for times before 1970. Turning that into a
        // panic or a wrapped-around number would both be worse than "unknown".
        let before = SystemTime::UNIX_EPOCH - Duration::from_secs(60);
        assert_eq!(to_millis(Some(before)), None);
    }

    #[test]
    fn kinds_map_across_the_boundary() {
        assert_eq!(Kind::from(EntryKind::File), Kind::File);
        assert_eq!(Kind::from(EntryKind::Directory), Kind::Directory);
        assert_eq!(Kind::from(EntryKind::Symlink), Kind::Symlink);
        assert_eq!(Kind::from(EntryKind::Other), Kind::Other);
    }

    #[test]
    fn directory_entries_carry_their_metadata() {
        let e = Entry::new("movie.mkv", Metadata::file(1024));
        let ffi = DirEntry::from(e);
        assert_eq!(ffi.name, "movie.mkv");
        assert_eq!(ffi.metadata.len, 1024);
        assert_eq!(ffi.metadata.kind, Kind::File);
    }

    #[test]
    fn open_flags_map_to_the_kernel_modes() {
        assert_eq!(OpenMode::from(OpenFlags::read_only()), OpenMode::read());
        assert_eq!(OpenMode::from(OpenFlags::write_only()), OpenMode::write());
        assert_eq!(OpenMode::from(OpenFlags::read_write()), OpenMode::read_write());
        assert_eq!(
            OpenMode::from(OpenFlags::create_exclusive()),
            OpenMode::create_new()
        );
    }

    #[test]
    fn truncation_survives_the_mapping() {
        let m = OpenMode::from(OpenFlags::write_only().with_truncate());
        assert!(m.truncates());
        assert!(m.is_write());
    }

    #[test]
    fn a_degenerate_flag_combination_is_read_only_rather_than_a_panic() {
        // The Kotlin side should not be able to produce this, but if it does,
        // opening read-only is the harmless reading.
        let flags = OpenFlags {
            read: false,
            write: false,
            create: true,
            create_new: false,
            truncate: false,
        };
        assert_eq!(OpenMode::from(flags), OpenMode::read());
    }

    #[test]
    fn connect_request_carries_seal_into_options() {
        let (uri, _, options) = ConnectRequest::new("smb://h/s").into_parts();
        assert_eq!(uri, "smb://h/s");
        assert_eq!(options.get_bool(krystallos_smb::OPT_SEAL), None, "off by default");

        let (_, _, options) = ConnectRequest::new("smb://h/s")
            .with_smb_seal(true)
            .into_parts();
        assert_eq!(options.get_bool(krystallos_smb::OPT_SEAL), Some(true));
    }

    #[test]
    fn connect_request_keeps_credentials_out_of_the_uri() {
        let (uri, creds, _) = ConnectRequest::new("smb://h/s")
            .with_credentials("alice", "hunter2")
            .into_parts();
        assert_eq!(uri, "smb://h/s");
        assert!(!uri.contains("hunter2"));
        assert_eq!(creds.username.as_deref(), Some("alice"));
        assert_eq!(creds.password.as_deref(), Some("hunter2"));
    }

    #[test]
    fn a_byte_range_reports_its_length() {
        let r = ByteRange::new(100, vec![0u8; 7]);
        assert_eq!(r.offset, 100);
        assert_eq!(r.len(), 7);
        assert!(!r.is_empty());

        let empty = ByteRange::new(100, Vec::new());
        assert!(empty.is_empty(), "an empty read means end-of-file");
    }
}
