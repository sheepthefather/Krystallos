use crate::{Capabilities, Entry, Error, Metadata, Result, VfsPath};
use async_trait::async_trait;

/// How to open a file.
///
/// Modelled on the POSIX open flags because every protocol worth supporting
/// maps onto them, but exposed as a struct rather than a bitmask so that
/// invalid combinations are hard to express.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpenMode {
    read: bool,
    write: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
}

impl OpenMode {
    /// Open an existing file for reading.
    pub const fn read() -> Self {
        OpenMode {
            read: true,
            write: false,
            create: false,
            create_new: false,
            truncate: false,
        }
    }

    /// Open for writing, creating the file if it does not exist. The file is
    /// **not** truncated — writes land at whatever offsets the caller asks for.
    pub const fn write() -> Self {
        OpenMode {
            read: false,
            write: true,
            create: true,
            create_new: false,
            truncate: false,
        }
    }

    /// Open for reading and writing, creating the file if absent.
    pub const fn read_write() -> Self {
        OpenMode {
            read: true,
            write: true,
            create: true,
            create_new: false,
            truncate: false,
        }
    }

    /// Create a new file, failing if it already exists.
    ///
    /// The right mode for an upload that must not clobber: it turns a
    /// check-then-write race into a single call the server arbitrates.
    pub const fn create_new() -> Self {
        OpenMode {
            read: false,
            write: true,
            create: true,
            create_new: true,
            truncate: false,
        }
    }

    pub const fn is_read(&self) -> bool {
        self.read
    }

    pub const fn is_write(&self) -> bool {
        self.write
    }

    pub const fn creates(&self) -> bool {
        self.create
    }

    pub const fn must_not_exist(&self) -> bool {
        self.create_new
    }

    pub const fn truncates(&self) -> bool {
        self.truncate
    }

    /// Same access mode, but discard any existing contents on open.
    pub const fn with_truncate(mut self) -> Self {
        self.truncate = true;
        self
    }
}

impl Default for OpenMode {
    fn default() -> Self {
        OpenMode::read()
    }
}

/// An open file on a backend.
///
/// All I/O is positioned: there is no shared cursor, so concurrent reads from
/// several tasks are safe and no locking is needed. That also makes this a
/// direct match for what media players want — they seek constantly and never
/// read strictly forward.
///
/// # Closing
///
/// There is no `Drop`-based cleanup. [`FileHandle::close`] must be called
/// explicitly, because releasing a handle can require a network round-trip and
/// `Drop` cannot await. Dropping a handle without closing it may leave the
/// server-side handle open until the session ends — SMB in particular keeps
/// such handles alive.
#[async_trait]
pub trait FileHandle: Send + Sync {
    /// Read into `buf` starting at `offset`.
    ///
    /// Returns the number of bytes read, which may be fewer than `buf.len()`.
    /// Returns `Ok(0)` at and past end-of-file — end-of-file is not an error.
    ///
    /// Reading entirely past the end yields `Ok(0)`; a read that straddles the
    /// end returns the bytes that exist.
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Write `buf` at `offset`, returning the number of bytes written.
    ///
    /// Like [`FileHandle::read_at`], a short write is possible and the caller
    /// must loop if it needs all bytes written.
    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize>;

    /// Truncate or extend the file to `len` bytes.
    async fn set_len(&self, len: u64) -> Result<()>;

    /// Push buffered writes to the backend.
    ///
    /// Whether this reaches durable storage depends on
    /// [`Capabilities::durable_flush`](crate::Capabilities::durable_flush).
    async fn flush(&self) -> Result<()>;

    /// Release the handle. Consumes nothing, so it can be called through a
    /// trait object; calling it twice must be harmless.
    async fn close(&self) -> Result<()>;

    /// The file's size as observed when the handle was opened.
    ///
    /// Not refreshed as the file changes: for the current size, `stat` the
    /// path. Kept synchronous and cheap because callers need it to size
    /// buffers, and a network round-trip there would be a nasty surprise.
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fill `buf` completely, looping over short reads.
    ///
    /// Returns `UnexpectedEof` if the file ends before `buf` is full. Provided
    /// here so each backend does not have to reimplement the loop, and so the
    /// short-read handling is consistent everywhere.
    async fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = self.read_at(offset + done as u64, &mut buf[done..]).await?;
            if n == 0 {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "file ended after {} of {} bytes at offset {}",
                        done,
                        buf.len(),
                        offset
                    ),
                )));
            }
            done += n;
        }
        Ok(())
    }
}

/// A connected storage backend.
///
/// One instance represents one live session against one endpoint. Backends are
/// expected to be cheap to share (`&self` everywhere) and safe to call into
/// concurrently, since several tasks may be reading different files at once.
///
/// # Implementing this
///
/// Implementations must not leak their protocol's vocabulary through these
/// signatures. If a concept has no portable equivalent, keep it inside the
/// backend and expose it via a separate extension trait — widening this one
/// makes every other backend pay for a feature it may not have.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// The endpoint this backend is connected to, suitable for
    /// [`VfsPath::to_uri`]. No trailing slash.
    fn endpoint(&self) -> &str;

    /// What this backend can do. Defaults to the conservative baseline, so a
    /// backend that forgets to override it under-promises rather than
    /// over-promises.
    fn capabilities(&self) -> Capabilities {
        Capabilities::MINIMAL
    }

    /// List a directory.
    ///
    /// Entries carry their metadata already — see [`Entry`] for why. The
    /// backend should not issue a `stat` per entry.
    async fn list(&self, path: &VfsPath) -> Result<Vec<Entry>>;

    async fn stat(&self, path: &VfsPath) -> Result<Metadata>;

    async fn open(&self, path: &VfsPath, mode: OpenMode) -> Result<Box<dyn FileHandle>>;

    /// Delete a file.
    ///
    /// Separate from [`StorageBackend::remove_dir`] because the underlying
    /// protocols distinguish them, and a unified `remove` would force a `stat`
    /// first — an extra round-trip on every deletion.
    async fn remove_file(&self, path: &VfsPath) -> Result<()>;

    /// Delete an empty directory. Fails on a non-empty one; recursive deletion
    /// is the caller's job, so that it stays visible in the caller's own error
    /// handling rather than being buried here.
    async fn remove_dir(&self, path: &VfsPath) -> Result<()>;

    /// Rename or move within the backend.
    ///
    /// Whether this is atomic is declared by
    /// [`Capabilities::atomic_rename`](crate::Capabilities::atomic_rename).
    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()>;

    /// Create a directory.
    ///
    /// Only single-level: the parent must exist. Backends should not
    /// create intermediate directories, so that a typo in a path surfaces
    /// instead of silently building a wrong tree.
    async fn mkdir(&self, path: &VfsPath) -> Result<()>;

    /// Tear down the session.
    ///
    /// After this returns, all other methods may fail. Calling it twice must be
    /// harmless. Unlike [`FileHandle::close`] this consumes nothing, so it can
    /// be invoked through a trait object.
    async fn shutdown(&self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_mode_is_read_only() {
        let m = OpenMode::read();
        assert!(m.is_read() && !m.is_write());
        assert!(!m.creates() && !m.truncates() && !m.must_not_exist());
    }

    #[test]
    fn write_mode_creates_but_does_not_truncate() {
        // Deliberate: a positioned writer must be able to fill a file out of
        // order. Truncation is opt-in via `with_truncate`.
        let m = OpenMode::write();
        assert!(m.is_write() && !m.is_read());
        assert!(m.creates() && !m.truncates());
    }

    #[test]
    fn create_new_refuses_to_clobber_but_still_creates() {
        let m = OpenMode::create_new();
        assert!(m.creates() && m.must_not_exist());
    }

    #[test]
    fn with_truncate_preserves_the_rest_of_the_mode() {
        let m = OpenMode::read_write().with_truncate();
        assert!(m.is_read() && m.is_write());
        assert!(m.truncates() && m.creates());
    }

    #[test]
    fn default_open_mode_is_read() {
        assert_eq!(OpenMode::default(), OpenMode::read());
    }
}
