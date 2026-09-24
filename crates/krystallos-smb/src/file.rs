//! A file open on an SMB session.
//!
//! The handle the caller holds is an opaque id, not a pointer. That is forced
//! by libsmb2's threading model: a `smb2fh` lives inside the session context,
//! which belongs to one thread, so it can never be handed out. Every operation
//! here sends a command to the session thread and waits for the answer.
//!
//! The cost is one copy in each direction — the bytes are moved through the
//! channel — which is negligible next to the network round-trip that surrounds
//! it.

use crate::actor::Session;
use async_trait::async_trait;
use krystallos_core::{Error, FileHandle, Result, VfsPath};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct RemoteFile {
    session: Session,
    id: u64,
    /// Size as last known. Updated by writes and truncation so that `len()`
    /// stays useful without a round-trip; not authoritative for reads, which
    /// run until the backend reports end-of-file.
    len: AtomicU64,
    closed: AtomicBool,
    path: VfsPath,
}

impl RemoteFile {
    pub(crate) fn new(session: Session, id: u64, len: u64, path: VfsPath) -> Self {
        RemoteFile {
            session,
            id,
            len: AtomicU64::new(len),
            closed: AtomicBool::new(false),
            path,
        }
    }

    fn closed_error(&self) -> Error {
        Error::Backend {
            message: format!("{} is already closed", self.path),
        }
    }
}

#[async_trait]
impl FileHandle for RemoteFile {
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }

        // Never ask for more than the negotiated dialect allows in one READ;
        // libsmb2 does not split oversized requests for us. The trait permits a
        // short read, so the caller loops — which `read_exact_at` already does.
        let want = buf.len().min(self.session.max_read_size() as usize);
        let data = self.session.read_at(self.id, offset, want as u32).await?;

        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }

        let want = buf.len().min(self.session.max_write_size() as usize);
        let written = self
            .session
            .write_at(self.id, offset, buf[..want].to_vec())
            .await?;

        // A write past the old end extends the file, so the cached length has
        // to grow with it.
        let end = offset + written as u64;
        self.len.fetch_max(end, Ordering::AcqRel);
        Ok(written)
    }

    async fn set_len(&self, len: u64) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        self.session.set_len(self.id, len).await?;
        self.len.store(len, Ordering::Release);
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        self.session.flush(self.id).await
    }

    async fn close(&self) -> Result<()> {
        // Idempotent: the trait requires it, and `Drop` may have got here
        // first. The swap makes exactly one caller do the work.
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.session.close(self.id).await
    }

    fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }
}

impl Drop for RemoteFile {
    fn drop(&mut self) {
        // Best effort, and deliberately not awaited: `Drop` cannot be async, and
        // silently leaking a server-side handle until the session ends is worse
        // than firing and forgetting.
        //
        // The close is queued synchronously on the command channel rather than
        // spawned as a task. Through the FFI this `Drop` runs on whatever thread
        // released the Kotlin object — a finalizer thread, typically — where
        // there is no tokio runtime to spawn onto, and a spawn-based close was
        // silently skipped there. Queuing needs no runtime. If the session
        // thread has already exited the send fails, which is fine:
        // `OpenFiles::close_all` released everything on the way out.
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.session.close_detached(self.id);
        }
    }
}

// There are no unit tests here on purpose. Everything worth checking about a
// remote file — that a handle outlives the call which created it, that closing
// twice is harmless, that a read after close fails rather than returning zeros
// — needs a real session to check against, so it lives in
// `tests/live_share.rs` rather than behind a fake that could agree with a
// wrong implementation.
