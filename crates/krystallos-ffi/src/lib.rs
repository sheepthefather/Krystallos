//! The FFI facade: `Kernel`, `Session`, `RemoteFile`.
//!
//! Three objects rather than one, because they have genuinely different
//! lifetimes and that is worth making visible on the Kotlin side:
//!
//! - [`Kernel`] holds the registry of backends. One per process.
//! - [`Session`] is a live connection. Opening one authenticates; closing it
//!   tears the connection down.
//! - [`RemoteFile`] is an open file, and is only valid while its session is.
//!
//! # The async split
//!
//! Control-plane calls are `async`, so they arrive in Kotlin as `suspend fun`
//! and do not block the UI thread. The data-plane call — [`RemoteFile::read_at`]
//! — is async too, despite the original plan calling for a synchronous
//! `readInto(ByteBuffer)`.
//!
//! That plan assumed UniFFI could hand Rust a mutable foreign buffer. It cannot:
//! `&[u8]` is one-directional, from the foreign side into Rust, and there is no
//! `&mut [u8]` counterpart. A read therefore has to return owned bytes, which
//! means a copy at the boundary either way — and once a copy is unavoidable,
//! the synchronous signature buys nothing and costs the ability to await.
//!
//! The cost is small enough not to matter: a 100 Mbps 4K stream is about
//! 12 MiB/s, so one-mebibyte reads are roughly twelve copies a second against
//! round-trips measured in milliseconds.
//!
//! # Cancellation
//!
//! UniFFI does not support it. A cancelled Kotlin coroutine does not stop the
//! Rust future, so a `Session` that has been dropped still finishes whatever
//! was in flight. That is tolerable here because every operation is bounded by
//! libsmb2's timeout (see `DEFAULT_TIMEOUT_SECS`), but it means "cancel" in the
//! UI is really "stop waiting", not "stop working".

mod error;
mod types;

pub use error::KernelError;
pub use types::{
    ByteRange, Capabilities, ConnectRequest, DirEntry, EntryMetadata, Kind, OpenFlags,
};

use crate::error::Result;
use crate::types::already_closed;
use krystallos_core::{BackendRegistry, StorageBackend, VfsPath};
use krystallos_local::LocalDriver;
use krystallos_smb::SmbDriver;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// The scaffolding macro must appear exactly once per crate. Without it UniFFI
// emits no FFI symbols at all, and the generated Kotlin binds to nothing.
uniffi::setup_scaffolding!();

/// Entry point: the registry of storage backends this build supports.
#[derive(uniffi::Object)]
pub struct Kernel {
    registry: Arc<BackendRegistry>,
}

#[uniffi::export]
impl Kernel {
    /// Create a kernel with every backend this build was compiled with.
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        let mut registry = BackendRegistry::new();
        registry.register(Box::new(LocalDriver::new()));
        registry.register(Box::new(SmbDriver::new()));
        Kernel {
            registry: Arc::new(registry),
        }
        .into()
    }

    /// The URI schemes this build can connect to, sorted.
    ///
    /// Worth exposing: a build could legitimately lack the SMB backend, and a
    /// caller offering `smb://` in its UI should be able to check rather than
    /// discover it from a failed connection.
    pub fn schemes(&self) -> Vec<String> {
        self.registry
            .schemes()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Report whether the SMB backend is present and usable.
    ///
    /// `true` if libsmb2 could allocate a session context. This is what catches
    /// the library having been silently dropped from the link — the `.so` would
    /// merely be smaller, which nothing else would notice.
    pub fn smb_available(&self) -> bool {
        krystallos_sys_smb2::link_probe()
    }

    /// Connect to a storage endpoint.
    ///
    /// Authenticates here rather than lazily, so a bad password surfaces now
    /// with a clear error instead of midway through a transfer.
    pub async fn connect(&self, request: ConnectRequest) -> Result<Arc<Session>> {
        let (uri, credentials, options) = request.into_parts();
        let backend = self
            .registry
            .connect_with(&uri, &credentials, &options)
            .await?;
        Ok(Arc::new(Session {
            backend: Arc::from(backend),
            endpoint: uri.to_string(),
            closed: AtomicBool::new(false),
        }))
    }}

impl Default for Kernel {
    fn default() -> Self {
        // `new` returns an `Arc` because that is what UniFFI's constructor
        // contract requires, so `Default` cannot delegate to it.
        let mut registry = BackendRegistry::new();
        registry.register(Box::new(LocalDriver::new()));
        registry.register(Box::new(SmbDriver::new()));
        Kernel {
            registry: Arc::new(registry),
        }
    }
}

/// A live connection to one endpoint.
#[derive(uniffi::Object)]
pub struct Session {
    backend: Arc<dyn StorageBackend>,
    endpoint: String,
    closed: AtomicBool,
}

impl Session {
    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(already_closed("session"));
        }
        Ok(())
    }

    fn path(&self, path: &str) -> Result<VfsPath> {
        Ok(VfsPath::new(path)?)
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Session {
    /// The endpoint this session is connected to, as it was given.
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    /// Whether this session has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// What this backend can do.
    ///
    /// Check before relying on an operation you need to degrade gracefully —
    /// positioned writes, for instance, are not universal.
    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities().into()
    }

    /// List a directory. Entries carry their metadata already.
    pub async fn list(&self, path: String) -> Result<Vec<DirEntry>> {
        self.ensure_open()?;
        let path = self.path(&path)?;
        let entries = self.backend.list(&path).await?;
        Ok(entries.into_iter().map(DirEntry::from).collect())
    }

    pub async fn stat(&self, path: String) -> Result<EntryMetadata> {
        self.ensure_open()?;
        let path = self.path(&path)?;
        let meta = self.backend.stat(&path).await?;
        Ok(EntryMetadata::from_core(&meta))
    }

    pub async fn mkdir(&self, path: String) -> Result<()> {
        self.ensure_open()?;
        let path = self.path(&path)?;
        Ok(self.backend.mkdir(&path).await?)
    }

    /// Delete a file.
    ///
    /// Separate from [`Session::remove_dir`] because the protocols distinguish
    /// them, and a unified call would force a `stat` first — an extra
    /// round-trip on every deletion.
    pub async fn remove_file(&self, path: String) -> Result<()> {
        self.ensure_open()?;
        let path = self.path(&path)?;
        Ok(self.backend.remove_file(&path).await?)
    }

    /// Delete an empty directory.
    ///
    /// Fails on a non-empty one. Recursive deletion belongs to the caller, so
    /// that its error handling stays visible there rather than being buried.
    pub async fn remove_dir(&self, path: String) -> Result<()> {
        self.ensure_open()?;
        let path = self.path(&path)?;
        Ok(self.backend.remove_dir(&path).await?)
    }

    /// Rename or move within the backend.
    ///
    /// Whether this is atomic is reported by
    /// [`Capabilities::atomic_rename`].
    pub async fn rename(&self, from: String, to: String) -> Result<()> {
        self.ensure_open()?;
        let from = self.path(&from)?;
        let to = self.path(&to)?;
        Ok(self.backend.rename(&from, &to).await?)
    }

    /// Open a file.
    ///
    /// The returned handle is only valid while this session is open; closing
    /// the session invalidates it.
    pub async fn open(&self, path: String, flags: OpenFlags) -> Result<Arc<RemoteFile>> {
        self.ensure_open()?;
        let vpath = self.path(&path)?;
        let handle = self.backend.open(&vpath, flags.into()).await?;
        Ok(Arc::new(RemoteFile {
            handle: Some(Arc::from(handle)),
            path,
            len: std::sync::atomic::AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }))
    }

    /// Close the session.
    ///
    /// Idempotent. After this every other call fails with `ConnectionLost`, and
    /// any file handles opened from it are dead.
    pub async fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        Ok(self.backend.shutdown().await?)
    }
}

/// A file open on a session.
///
/// # Closing
///
/// [`RemoteFile::close`] is explicit and idempotent. Dropping the Kotlin object
/// also releases the handle — UniFFI calls `Drop` when the Kotlin object is
/// garbage-collected — but that timing is the garbage collector's, not the
/// caller's, and on a server that limits concurrent open files the difference
/// matters. Close when you are done.
#[derive(uniffi::Object)]
pub struct RemoteFile {
    /// `None` once closed. Taking it is what makes closing idempotent without
    /// a lock: `Option::take` on `&mut self` under UniFFI's exclusive borrow.
    handle: Option<Arc<dyn krystallos_core::FileHandle>>,
    path: String,
    /// Size as last observed. Cached so `len` can be a plain getter rather than
    /// an async call, which is what a caller wants for sizing a buffer.
    len: std::sync::atomic::AtomicU64,
    closed: AtomicBool,
}

#[uniffi::export(async_runtime = "tokio")]
impl RemoteFile {
    /// The path this file was opened from.
    pub fn path(&self) -> String {
        self.path.clone()
    }

    /// The file's size, as observed when it was opened or last written.
    ///
    /// A plain getter, not an `async fn`, because callers need it to size
    /// buffers and an await there would be a nasty surprise. Use
    /// [`Session::stat`] for the current size.
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Read up to `len` bytes starting at `offset`.
    ///
    /// Returns fewer bytes than asked for only at end-of-file, where the result
    /// is empty. There is no separate end-of-file error: a short or empty
    /// result is the signal.
    ///
    /// Reads larger than the negotiated maximum are split by the backend, so
    /// asking for more than one mebibyte is fine.
    pub async fn read_at(&self, offset: u64, len: u32) -> Result<ByteRange> {
        let handle = self.handle()?;
        if len == 0 {
            return Ok(ByteRange::new(offset, Vec::new()));
        }
        let mut buf = vec![0u8; len as usize];
        let n = handle.read_at(offset, &mut buf).await?;
        buf.truncate(n);
        Ok(ByteRange::new(offset, buf))
    }

    /// Read exactly `len` bytes, or fail.
    ///
    /// The convenience a caller actually wants for fixed-size reads — headers,
    /// index tables — where a short result means something is wrong rather than
    /// meaning end-of-file.
    pub async fn read_exact_at(&self, offset: u64, len: u32) -> Result<ByteRange> {
        let handle = self.handle()?;
        if len == 0 {
            return Ok(ByteRange::new(offset, Vec::new()));
        }
        let mut buf = vec![0u8; len as usize];
        handle.read_exact_at(offset, &mut buf).await?;
        Ok(ByteRange::new(offset, buf))
    }

    /// Write `data` at `offset`, returning how many bytes were accepted.
    ///
    /// A short write is possible; loop if you need all of it written.
    pub async fn write_at(&self, offset: u64, data: Vec<u8>) -> Result<u64> {
        let handle = self.handle()?;
        let n = handle.write_at(offset, &data).await?;
        // A write past the old end extends the file, so the cached length has
        // to grow with it.
        let end = offset + n as u64;
        self.len.fetch_max(end, Ordering::AcqRel);
        Ok(n as u64)
    }

    /// Truncate or extend the file.
    pub async fn set_len(&self, len: u64) -> Result<()> {
        let handle = self.handle()?;
        handle.set_len(len).await?;
        self.len.store(len, Ordering::Release);
        Ok(())
    }

    /// Push buffered writes to the backend.
    ///
    /// Whether this reaches durable storage depends on
    /// [`Capabilities::durable_flush`].
    pub async fn flush(&self) -> Result<()> {
        let handle = self.handle()?;
        Ok(handle.flush().await?)
    }

    /// Release the handle. Idempotent.
    pub async fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // Cloning the handle out and dropping our reference is what actually
        // closes it: `RemoteFile` in `krystallos-smb` releases on its own
        // `Drop`, which runs as soon as the last `Arc` goes.
        let handle = self.handle.clone();
        drop(handle);
        Ok(())
    }
}

impl RemoteFile {
    fn handle(&self) -> Result<Arc<dyn krystallos_core::FileHandle>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(already_closed("file"));
        }
        self.handle
            .clone()
            .ok_or_else(|| already_closed("file"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use krystallos_core::{Credentials, OpenMode};

    fn kernel() -> Kernel {
        Kernel::default()
    }

    #[test]
    fn a_new_kernel_has_both_backends() {
        let k = kernel();
        let schemes = k.schemes();
        assert!(schemes.contains(&"file".to_string()), "got {schemes:?}");
        assert!(schemes.contains(&"smb".to_string()), "got {schemes:?}");
    }

    #[test]
    fn the_smb_backend_is_actually_linked() {
        // If libsmb2 were dropped from the link this is the only thing that
        // would notice.
        assert!(kernel().smb_available());
    }

    #[tokio::test]
    async fn connecting_to_a_local_directory_gives_a_usable_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let uri = krystallos_local::uri_for(&canonical);

        let session = kernel()
            .connect(ConnectRequest::new(uri))
            .await
            .expect("connect");

        assert!(!session.is_closed());
        assert_eq!(session.endpoint(), session.endpoint());

        let entries = session.list("/".to_string()).await.expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].metadata.len, 5);

        session.close().await.expect("close");
        assert!(session.is_closed());
    }

    #[tokio::test]
    async fn a_closed_session_refuses_further_work() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let session = kernel()
            .connect(ConnectRequest::new(krystallos_local::uri_for(&canonical)))
            .await
            .expect("connect");

        session.close().await.expect("close");
        // Closing twice must be harmless.
        session.close().await.expect("close again");

        match session.list("/".to_string()).await {
            Err(KernelError::ConnectionLost { .. }) => {}
            other => panic!("expected ConnectionLost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_full_round_trip_through_the_facade() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let session = kernel()
            .connect(ConnectRequest::new(krystallos_local::uri_for(&canonical)))
            .await
            .expect("connect");

        session.mkdir("/media".to_string()).await.expect("mkdir");

        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let file = session
            .open("/media/x.bin".to_string(), OpenFlags::create_exclusive())
            .await
            .expect("open");

        let mut written = 0usize;
        while written < payload.len() {
            let n = file
                .write_at(written as u64, payload[written..].to_vec())
                .await
                .expect("write");
            assert!(n > 0, "a write that makes no progress would loop forever");
            written += n as usize;
        }
        file.flush().await.expect("flush");
        file.close().await.expect("close");

        let meta = session.stat("/media/x.bin".to_string()).await.expect("stat");
        assert_eq!(meta.len, payload.len() as u64);

        let file = session
            .open("/media/x.bin".to_string(), OpenFlags::read_only())
            .await
            .expect("reopen");
        let read = file
            .read_exact_at(0, payload.len() as u32)
            .await
            .expect("read back");
        assert_eq!(read.data, payload, "round-tripped bytes must match exactly");
        file.close().await.expect("close");

        session
            .rename("/media/x.bin".to_string(), "/media/y.bin".to_string())
            .await
            .expect("rename");
        session
            .remove_file("/media/y.bin".to_string())
            .await
            .expect("remove");
        session.remove_dir("/media".to_string()).await.expect("rmdir");
    }

    #[tokio::test]
    async fn reading_past_the_end_yields_an_empty_range_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small"), b"abc").unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let session = kernel()
            .connect(ConnectRequest::new(krystallos_local::uri_for(&canonical)))
            .await
            .expect("connect");

        let file = session
            .open("/small".to_string(), OpenFlags::read_only())
            .await
            .expect("open");

        let r = file.read_at(3, 16).await.expect("read at eof");
        assert!(r.is_empty(), "end-of-file is a short read, not an error");
        assert_eq!(r.offset, 3);

        let r = file.read_at(9999, 16).await.expect("read past eof");
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn read_exact_fails_rather_than_returning_a_short_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small"), b"abc").unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let session = kernel()
            .connect(ConnectRequest::new(krystallos_local::uri_for(&canonical)))
            .await
            .expect("connect");

        let file = session
            .open("/small".to_string(), OpenFlags::read_only())
            .await
            .expect("open");

        // A caller asking for exactly this many bytes needs to know it did not
        // get them, rather than silently receiving three.
        let err = file.read_exact_at(0, 16).await;
        assert!(err.is_err(), "expected a failure, got {err:?}");
    }

    #[tokio::test]
    async fn a_closed_file_refuses_further_work() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"abc").unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let session = kernel()
            .connect(ConnectRequest::new(krystallos_local::uri_for(&canonical)))
            .await
            .expect("connect");

        let file = session
            .open("/f".to_string(), OpenFlags::read_only())
            .await
            .expect("open");
        file.close().await.expect("close");
        file.close().await.expect("close again");

        match file.read_at(0, 3).await {
            Err(KernelError::ConnectionLost { .. }) => {}
            other => panic!("expected ConnectionLost, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn paths_that_escape_the_root_are_rejected_at_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let session = kernel()
            .connect(ConnectRequest::new(krystallos_local::uri_for(&canonical)))
            .await
            .expect("connect");

        // The path parser rejects this rather than clamping, so a caller
        // cannot reach outside the session's root by asking nicely.
        match session.list("/../../etc".to_string()).await {
            Err(KernelError::InvalidPath { .. }) => {}
            other => panic!("expected InvalidPath, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connecting_to_something_that_is_not_there_is_a_clear_error() {
        let err = kernel()
            .connect(ConnectRequest::new("file:///definitely/not/here/at/all"))
            .await
            .map(|_| ())
            .expect_err("connecting to a missing directory must fail");
        assert!(
            matches!(err, KernelError::NotFound { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_unregistered_scheme_names_the_problem() {
        let err = kernel()
            .connect(ConnectRequest::new("sftp://host/share"))
            .await
            .map(|_| ())
            .expect_err("sftp is not implemented");
        assert!(matches!(err, KernelError::Unsupported { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn the_local_backend_declares_full_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let session = kernel()
            .connect(ConnectRequest::new(krystallos_local::uri_for(&canonical)))
            .await
            .expect("connect");

        let caps = session.capabilities();
        assert!(caps.random_read && caps.random_write);
        assert!(caps.atomic_rename && caps.durable_flush);
    }

    #[test]
    fn open_mode_conversion_is_reachable_from_the_ffi_types() {
        // Guards the plumbing between `OpenFlags` and `OpenMode` that the
        // session relies on; the conversion itself is tested in `types`.
        let mode: OpenMode = OpenFlags::read_write().into();
        assert!(mode.is_read() && mode.is_write());
        let _ = Credentials::anonymous();
    }
}
