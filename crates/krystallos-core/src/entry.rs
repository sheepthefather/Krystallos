/// What kind of thing an entry is.
///
/// Kept deliberately small. Protocols disagree wildly about the finer
/// distinctions (SMB has reparse points and attribute bits, WebDAV has almost
/// nothing, SFTP has the POSIX mode bits), so anything beyond this set would be
/// a lie for at least one backend.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum EntryKind {
    File,
    Directory,
    /// A symbolic link or protocol equivalent. Note that backends may or may not
    /// follow links when resolving a path; if the distinction matters to you,
    /// check [`Capabilities`](crate::Capabilities) and `stat` the target.
    Symlink,
    /// Something that is neither file nor directory and has no portable
    /// equivalent — an SMB named pipe, a device node, a socket.
    Other,
}

impl EntryKind {
    pub fn is_dir(self) -> bool {
        matches!(self, EntryKind::Directory)
    }

    pub fn is_file(self) -> bool {
        matches!(self, EntryKind::File)
    }
}

/// Portable metadata for a single entry.
///
/// Only fields that every backend can meaningfully populate appear here.
/// Protocol-specific attributes stay inside the backend; if a caller needs
/// them, the backend should expose them through its own extension trait rather
/// than widening this struct.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Metadata {
    pub kind: EntryKind,
    pub len: u64,
    /// Last modification time, if the backend reports one.
    pub modified: Option<std::time::SystemTime>,
    /// Creation time. Absent on backends that do not track it, and `None` is
    /// not an error — plenty of servers simply do not send it.
    pub created: Option<std::time::SystemTime>,
    pub accessed: Option<std::time::SystemTime>,
    /// Whether the entry is writable. Backends that cannot report this set it
    /// to `false` conservatively rather than claiming writability they cannot
    /// guarantee.
    pub read_only: bool,
}

impl Metadata {
    pub fn file(len: u64) -> Self {
        Metadata {
            kind: EntryKind::File,
            len,
            modified: None,
            created: None,
            accessed: None,
            read_only: false,
        }
    }

    pub fn directory() -> Self {
        Metadata {
            kind: EntryKind::Directory,
            len: 0,
            modified: None,
            created: None,
            accessed: None,
            read_only: false,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    pub fn is_file(&self) -> bool {
        self.kind.is_file()
    }
}

/// A directory listing entry: a name plus the metadata that came with it.
///
/// The metadata is carried inline on purpose. Splitting it out would invite
/// callers to `list` and then `stat` each result, which over a network is one
/// round-trip per entry — the single easiest way to make a file browser feel
/// broken on a 10k-file directory. Backends are expected to fill this in from
/// whatever their directory enumeration already returned.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The entry's name, exactly as the backend reported it.
    ///
    /// Callers must use this value verbatim when addressing the entry later.
    /// Reconstructing the name from user input or re-encoding it risks a
    /// mismatch on servers that store names in a different Unicode
    /// normalization form (macOS shares are the common case).
    pub name: String,
    pub metadata: Metadata,
}

impl Entry {
    pub fn new(name: impl Into<String>, metadata: Metadata) -> Self {
        Entry {
            name: name.into(),
            metadata,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.metadata.is_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_predicates() {
        assert!(EntryKind::Directory.is_dir());
        assert!(!EntryKind::Directory.is_file());
        assert!(EntryKind::File.is_file());
        assert!(!EntryKind::Symlink.is_file());
        assert!(!EntryKind::Symlink.is_dir());
    }

    #[test]
    fn metadata_constructors_set_kind_and_len() {
        let f = Metadata::file(4096);
        assert!(f.is_file() && !f.is_dir());
        assert_eq!(f.len, 4096);
        assert!(!f.read_only);

        let d = Metadata::directory();
        assert!(d.is_dir() && !d.is_file());
        assert_eq!(d.len, 0);
    }

    #[test]
    fn entry_exposes_its_metadata() {
        let e = Entry::new("movie.mkv", Metadata::file(1024));
        assert_eq!(e.name, "movie.mkv");
        assert!(!e.is_dir());
        assert_eq!(e.metadata.len, 1024);
    }
}
