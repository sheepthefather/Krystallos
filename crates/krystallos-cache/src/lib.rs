//! Read-ahead buffering.
//!
//! # What this is for, and what it is not
//!
//! It is not a throughput optimisation. Measured against a real share, the SMB
//! backend already reads at roughly 250 MiB/s with a 4 ms round-trip, and no
//! amount of buffering makes a network faster than the network. What buffering
//! changes is *latency*: without it, every read the player issues pays a full
//! round-trip before a single byte arrives. With it, a sequential read is
//! usually already in memory.
//!
//! That matters for a media player specifically because of how Media3 drives a
//! `DataSource`: it seeks, reads a little, seeks again, and reads again during
//! startup, then settles into a steady forward scan. The seeks during startup
//! are where a round-trip per read is visible as a slow start.
//!
//! # Shape
//!
//! [`ReadAhead`] wraps any [`FileHandle`] and serves reads from a window it
//! keeps ahead of the caller's position. It is a decorator, so backends do not
//! know it exists and callers do not have to opt in to the trait.
//!
//! # The design constraint that shapes everything
//!
//! `FileHandle` takes `&self`, not `&mut self`. Several tasks may be reading
//! through one handle concurrently, and a media player genuinely does this —
//! the extractor reads ahead while the player seeks. So the window cannot be
//! held behind a plain `&mut`, and it cannot be a lock held across an await
//! either, because that would serialise every read in the file behind whichever
//! one is currently fetching.
//!
//! The approach taken here is to hold the buffer in a `std::sync::Mutex` that
//! is **never held across an await**. A read takes the lock only long enough to
//! copy bytes out of the window, or to decide that the window does not cover
//! what is wanted. Fetching happens outside the lock, and the result is
//! published under it. Two concurrent readers may therefore both miss and both
//! fetch the same range; the second one to publish wins and the first is
//! discarded. That is wasted work, not incorrect work, and it is far simpler
//! than a single-flight coordination scheme whose benefit only appears under
//! concurrent random reads —which is not the access pattern this exists for.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use krystallos_core::{FileHandle, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// How much to keep buffered ahead of the caller.
///
/// One mebibyte, matching the transfer chunk the CLI and the SMB backend
/// already use. Larger windows amortise the round-trip further but cost memory
/// per open file, and a player may hold several open at once —the current
/// track, the next one being probed, and possibly a subtitle file.
pub const DEFAULT_WINDOW: usize = 1024 * 1024;

/// Read-ahead window over another [`FileHandle`].
pub struct ReadAhead<H> {
    inner: H,
    window_size: usize,
    state: Mutex<Window>,
    /// How many reads reached the wrapped handle.
    ///
    /// Counted because read-ahead's entire claim is about round-trip count, and
    /// the only way to check a claim like that is to count. Wall-clock timing
    /// cannot show it on a low-latency link, where transferring the bytes
    /// dominates and the round-trips are hidden —which is exactly the case
    /// where someone would otherwise conclude the layer does nothing.
    inner_reads: AtomicU64,
    /// Bytes asked of the wrapped handle.
    inner_bytes: AtomicU64,
}

/// The buffered region, and what is known about it.
#[derive(Default)]
struct Window {
    /// Bytes of the file starting at `start`.
    data: Vec<u8>,
    /// Offset the buffer begins at.
    start: u64,
    /// Set once a fetch has hit end-of-file, so a read past the end does not
    /// keep re-issuing requests that will come back empty.
    eof: bool,
}

impl Window {
    /// The half-open range of the file this window covers.
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    /// Whether the window already holds everything needed for `[offset, offset+len)`.
    ///
    /// Once a fetch has hit end-of-file, the window knows the file's full
    /// extent from its start: bytes up to `end` are in the buffer, and there is
    /// nothing beyond. So a request at or past `end` is answered without a
    /// fetch —the answer is zero bytes, and issuing a request to learn that
    /// again would be pointless.
    fn covers(&self, offset: u64, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        if offset < self.start {
            return false;
        }
        let want_end = offset.saturating_add(len as u64);
        want_end <= self.end() || self.eof
    }

    /// Copy out of the window.
    ///
    /// Returns zero bytes for any part of the request at or past the window's
    /// end, which is what makes a read past end-of-file come back empty rather
    /// than panic.
    fn copy_out(&self, offset: u64, buf: &mut [u8]) -> usize {
        // Clamped rather than trusted: an offset past the end of the buffer is
        // a legitimate request (it means end-of-file), and slicing
        // `data[from..]` unclamped would panic on it.
        let from = ((offset - self.start) as usize).min(self.data.len());
        let available = self.data.len() - from;
        let n = available.min(buf.len());
        buf[..n].copy_from_slice(&self.data[from..from + n]);
        n
    }
}

impl<H: FileHandle> ReadAhead<H> {
    /// Wrap `inner` with the [default window](DEFAULT_WINDOW).
    pub fn new(inner: H) -> Self {
        ReadAhead::with_window(inner, DEFAULT_WINDOW)
    }

    /// Wrap `inner` with a specific window size.
    ///
    /// A window of zero disables read-ahead entirely, which is useful for
    /// measuring what it is actually worth rather than assuming.
    pub fn with_window(inner: H, window_size: usize) -> Self {
        ReadAhead {
            inner,
            window_size,
            state: Mutex::new(Window::default()),
            inner_reads: AtomicU64::new(0),
            inner_bytes: AtomicU64::new(0),
        }
    }

    /// The handle underneath, for operations this wrapper does not intercept.
    pub fn inner(&self) -> &H {
        &self.inner
    }

    /// How many reads have reached the wrapped handle.
    ///
    /// The number read-ahead exists to reduce. Compare it against the number of
    /// `read_at` calls the caller made.
    pub fn inner_read_count(&self) -> u64 {
        self.inner_reads.load(Ordering::Acquire)
    }

    /// How many bytes have been asked of the wrapped handle.
    pub fn inner_byte_count(&self) -> u64 {
        self.inner_bytes.load(Ordering::Acquire)
    }

    /// Pass a read through to the handle, counting it.
    async fn read_through(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.inner_reads.fetch_add(1, Ordering::AcqRel);
        self.inner_bytes.fetch_add(buf.len() as u64, Ordering::AcqRel);
        self.inner.read_at(offset, buf).await
    }

    /// Drop the buffered window.
    ///
    /// Needed after a write: the buffer holds a copy of bytes that have now
    /// changed, and serving them afterwards would return stale data that looks
    /// perfectly valid.
    fn invalidate(&self) {
        let mut w = self.state.lock().expect("read-ahead window poisoned");
        w.data.clear();
        w.start = 0;
        w.eof = false;
    }

    /// Fetch a window starting at `start` and publish it.
    ///
    /// Called with the lock *not* held; the lock is taken only to publish.
    async fn fill(&self, start: u64) -> Result<()> {
        // Stop at the file's end when the size is known, so the last window is
        // not a full megabyte of which most is past the end.
        let len = self.window_size.min(self.remaining_from(start));

        let mut buf = vec![0u8; len];
        let n = self.read_through(start, &mut buf).await?;
        buf.truncate(n);
        let hit_eof = n == 0 || (len > 0 && n < len);

        let mut w = self.state.lock().expect("read-ahead window poisoned");
        w.start = start;
        w.data = buf;
        w.eof = hit_eof;
        Ok(())
    }

    /// How much of the file lies at or after `start`.
    ///
    /// `FileHandle::len` is the size observed when the handle was opened and is
    /// not refreshed, so it is advisory. It is used here only to size a fetch;
    /// correctness comes from the read returning what it returns.
    fn remaining_from(&self, start: u64) -> usize {
        let total = self.inner.len();
        if total <= start {
            // Either the file is genuinely shorter, or the length is stale and
            // too small. Falling back to a full window is the safe reading: a
            // short read costs one wasted request, whereas trusting a stale
            // length could truncate a window that had data in it.
            return self.window_size;
        }
        ((total - start) as usize).min(self.window_size)
    }

    /// Whether a fetch would actually do anything.
    fn worth_fetching(&self, offset: u64) -> bool {
        self.window_size > 0 && {
            let w = self.state.lock().expect("read-ahead window poisoned");
            !(w.eof && offset >= w.end())
        }
    }
}

#[async_trait]
impl<H: FileHandle + Send + Sync> FileHandle for ReadAhead<H> {
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        if self.window_size == 0 {
            // Read-ahead disabled. Straight through, so that measuring what
            // this layer is worth needs no separate code path.
            return self.read_through(offset, buf).await;
        }

        // Fast path: the window already has it. The lock is taken and released
        // with no await in between.
        {
            let w = self.state.lock().expect("read-ahead window poisoned");
            if w.covers(offset, buf.len()) {
                return Ok(w.copy_out(offset, buf));
            }
        }

        // A request spanning more than one window cannot be served from a
        // single window, so fetching one would not help. Hand it to the handle,
        // which splits it internally. This is what makes "ask for more than the
        // window and still get everything" work rather than silently truncating.
        if buf.len() > self.window_size {
            return self.read_through(offset, buf).await;
        }

        if !self.worth_fetching(offset) {
            // The window says the file ends before here; go straight to the
            // handle so the answer is still authoritative.
            return self.read_through(offset, buf).await;
        }

        // Fetch the window that *contains* the requested offset, not one that
        // merely starts there. A caller reading 1 MiB at a time from offset 0
        // would otherwise ask for a window at 0, then at 1 MiB, and so on —
        // each one a fresh round-trip and no read-ahead at all, which is the
        // opposite of the point.
        let window_start = (offset / self.window_size as u64) * self.window_size as u64;
        self.fill(window_start).await?;

        // Serve from what was just fetched.
        //
        // The guard is scoped to this block and dropped before the fallback
        // below, so no lock is ever held across an await —which would make
        // this future non-`Send` and, worse, serialise every concurrent reader
        // behind whichever one is fetching.
        let served = {
            let w = self.state.lock().expect("read-ahead window poisoned");
            w.covers(offset, buf.len()).then(|| w.copy_out(offset, buf))
        };
        if let Some(n) = served {
            return Ok(n);
        }

        // The fetch landed somewhere that does not contain the request —only
        // possible if the handle was replaced underneath us. Fall back to a
        // direct read rather than returning something wrong.
        self.inner.read_at(offset, buf).await
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        // A write invalidates the window: it now holds bytes that no longer
        // match the file.
        let n = self.inner.write_at(offset, buf).await?;
        self.invalidate();
        Ok(n)
    }

    async fn set_len(&self, len: u64) -> Result<()> {
        self.inner.set_len(len).await?;
        self.invalidate();
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        self.inner.flush().await
    }

    async fn close(&self) -> Result<()> {
        self.invalidate();
        self.inner.close().await
    }

    fn len(&self) -> u64 {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use krystallos_core::Error;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A handle that serves `data` and counts how many reads it was asked for.
    ///
    /// The counter is the point of most of these tests: read-ahead is a claim
    /// about how many round-trips a sequence of reads costs, and the only way
    /// to check a claim like that is to count.
    struct CountingHandle {
        data: Vec<u8>,
        reads: AtomicUsize,
        bytes_requested: AtomicUsize,
    }

    impl CountingHandle {
        fn new(data: Vec<u8>) -> Self {
            CountingHandle {
                data,
                reads: AtomicUsize::new(0),
                bytes_requested: AtomicUsize::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::Acquire)
        }
    }

    #[async_trait]
    impl FileHandle for CountingHandle {
        async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            self.bytes_requested.fetch_add(buf.len(), Ordering::AcqRel);

            let start = (offset as usize).min(self.data.len());
            let available = self.data.len() - start;
            let n = available.min(buf.len());
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            Ok(n)
        }

        async fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize> {
            Err(Error::backend("not writable"))
        }
        async fn set_len(&self, _len: u64) -> Result<()> {
            Err(Error::backend("not writable"))
        }
        async fn flush(&self) -> Result<()> {
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
        fn len(&self) -> u64 {
            self.data.len() as u64
        }
    }

    fn payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn sequential_reads_cost_one_fetch_per_window() {
        // The whole claim of this module in one test: reading a file in small
        // steps must not cost a round-trip per step.
        let data = payload(4 * 1024 * 1024);
        let handle = ReadAhead::with_window(CountingHandle::new(data.clone()), 1024 * 1024);

        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = handle.read_at(out.len() as u64, &mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }

        assert_eq!(out, data, "read-ahead must not alter the bytes");
        let reads = handle.inner().reads();
        assert!(
            reads <= 5,
            "4 MiB read in 4 KiB steps should cost ~4 fetches, took {reads}"
        );
    }

    #[tokio::test]
    async fn without_read_ahead_the_same_loop_is_much_worse() {
        // The baseline the previous test is measured against. If this ever
        // stops being dramatically larger, the test above has stopped proving
        // anything.
        let data = payload(1024 * 1024);
        let handle = CountingHandle::new(data.clone());

        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = handle.read_at(out.len() as u64, &mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }

        assert_eq!(out, data);
        assert_eq!(
            handle.reads(),
            257,
            "one round-trip per 4 KiB read, plus the one that reports end-of-file"
        );
    }

    #[tokio::test]
    async fn a_read_larger_than_the_window_returns_everything() {
        // A window smaller than the request must not truncate the answer.
        let data = payload(64 * 1024);
        let handle = ReadAhead::with_window(CountingHandle::new(data.clone()), 4096);

        let mut buf = vec![0u8; 32 * 1024];
        let n = handle.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, buf.len(), "a short read here would lose data");
        assert_eq!(buf, data[..n]);
    }

    #[tokio::test]
    async fn reading_past_the_end_yields_zero_and_stops_fetching() {
        let data = payload(10_000);
        let handle = ReadAhead::with_window(CountingHandle::new(data.clone()), 4096);

        // Read it all first so the window has been filled.
        let mut all = vec![0u8; data.len()];
        handle.read_exact_at(0, &mut all).await.unwrap();
        assert_eq!(all, data);
        let before = handle.inner().reads();

        // The first read past the end costs one probe: the window cannot know
        // the file ended without asking, because a fetch that returns exactly
        // the bytes it asked for is indistinguishable from one that stopped
        // short of a longer file. That single round-trip is unavoidable.
        let mut buf = [0u8; 64];
        assert_eq!(handle.read_at(999_999, &mut buf).await.unwrap(), 0);
        assert_eq!(
            handle.inner().reads(),
            before + 1,
            "learning that the file ended should cost one probe"
        );

        // Everything after that must be answered from what was learned.
        for _ in 0..10 {
            assert_eq!(handle.read_at(999_999, &mut buf).await.unwrap(), 0);
        }
        assert_eq!(
            handle.inner().reads(),
            before + 1,
            "repeated reads past the end must not keep hitting the handle"
        );
    }

    #[tokio::test]
    async fn a_seek_backwards_is_served_from_the_window() {
        // Media3 seeks backwards constantly during startup; a window that only
        // ever moved forward would make each of those a round-trip.
        let data = payload(256 * 1024);
        let handle = ReadAhead::with_window(CountingHandle::new(data.clone()), 1024 * 1024);

        let mut buf = [0u8; 1024];
        handle.read_exact_at(100_000, &mut buf).await.unwrap();
        let after_first = handle.inner().reads();

        for offset in [0u64, 50_000, 100_000, 200_000, 0] {
            let mut b = [0u8; 512];
            handle.read_exact_at(offset, &mut b).await.unwrap();
            assert_eq!(
                b,
                data[offset as usize..offset as usize + 512],
                "wrong bytes at offset {offset}"
            );
        }
        assert_eq!(
            handle.inner().reads(),
            after_first,
            "backwards seeks inside the window must not refetch"
        );
    }

    #[tokio::test]
    async fn a_fetch_is_aligned_to_the_window_not_to_the_request() {
        // A caller reading one window at a time starting at 0 would otherwise
        // fetch a window at 0, then at window_size, and so on —each a fresh
        // round-trip and no read-ahead at all.
        let data = payload(4 * 1024 * 1024);
        let handle = ReadAhead::with_window(CountingHandle::new(data.clone()), 1024 * 1024);

        let mut buf = vec![0u8; 1024 * 1024];
        for i in 0..4u64 {
            let n = handle
                .read_at(i * 1024 * 1024, &mut buf)
                .await
                .unwrap();
            assert_eq!(n, buf.len());
        }
        assert_eq!(
            handle.inner().reads(),
            4,
            "four aligned full-window reads should be four fetches"
        );
    }

    #[tokio::test]
    async fn a_zero_sized_window_disables_read_ahead() {
        // Needed to measure what this is worth rather than assume it.
        let data = payload(64 * 1024);
        let handle = ReadAhead::with_window(CountingHandle::new(data.clone()), 0);

        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = handle.read_at(out.len() as u64, &mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, data, "disabling read-ahead must not change the bytes");
        assert_eq!(
            handle.inner().reads(),
            17,
            "one read per request when disabled, plus the end-of-file probe"
        );
    }

    #[tokio::test]
    async fn the_window_never_asks_for_more_than_the_file_holds() {
        // The last window would otherwise be a full megabyte of which most is
        // past the end.
        let data = payload(1000);
        let handle = ReadAhead::with_window(CountingHandle::new(data.clone()), 1024 * 1024);

        let mut buf = vec![0u8; 1000];
        handle.read_exact_at(0, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        let requested = handle.inner().bytes_requested.load(Ordering::Acquire);
        assert!(
            requested <= 1000,
            "asked the handle for {requested} bytes of a 1000-byte file"
        );
    }

    #[tokio::test]
    async fn a_stale_length_does_not_truncate_a_read() {
        // `len()` is the size seen at open and is not refreshed. If it is too
        // small, sizing a fetch from it must not lose data.
        struct LyingHandle(Vec<u8>);

        #[async_trait]
        impl FileHandle for LyingHandle {
            async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
                let start = (offset as usize).min(self.0.len());
                let n = (self.0.len() - start).min(buf.len());
                buf[..n].copy_from_slice(&self.0[start..start + n]);
                Ok(n)
            }
            async fn write_at(&self, _o: u64, _b: &[u8]) -> Result<usize> {
                Err(Error::backend("read-only"))
            }
            async fn set_len(&self, _l: u64) -> Result<()> {
                Err(Error::backend("read-only"))
            }
            async fn flush(&self) -> Result<()> {
                Ok(())
            }
            async fn close(&self) -> Result<()> {
                Ok(())
            }
            fn len(&self) -> u64 {
                // Claims to be empty while actually holding data.
                0
            }
        }

        let data = payload(8192);
        let handle = ReadAhead::with_window(LyingHandle(data.clone()), 4096);

        let mut buf = vec![0u8; 8192];
        handle.read_exact_at(0, &mut buf).await.unwrap();
        assert_eq!(buf, data, "a stale length must not truncate the result");
    }

    #[tokio::test]
    async fn a_write_invalidates_the_window() {
        // Serving buffered bytes after they changed would return stale data
        // that looks perfectly valid, which is the worst kind of wrong.
        struct MemHandle(Mutex<Vec<u8>>);

        #[async_trait]
        impl FileHandle for MemHandle {
            async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
                let d = self.0.lock().unwrap();
                let start = (offset as usize).min(d.len());
                let n = (d.len() - start).min(buf.len());
                buf[..n].copy_from_slice(&d[start..start + n]);
                Ok(n)
            }
            async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
                let mut d = self.0.lock().unwrap();
                let end = offset as usize + buf.len();
                if d.len() < end {
                    d.resize(end, 0);
                }
                d[offset as usize..end].copy_from_slice(buf);
                Ok(buf.len())
            }
            async fn set_len(&self, len: u64) -> Result<()> {
                self.0.lock().unwrap().resize(len as usize, 0);
                Ok(())
            }
            async fn flush(&self) -> Result<()> {
                Ok(())
            }
            async fn close(&self) -> Result<()> {
                Ok(())
            }
            fn len(&self) -> u64 {
                self.0.lock().unwrap().len() as u64
            }
        }

        let handle = ReadAhead::with_window(MemHandle(Mutex::new(payload(8192))), 4096);

        let mut buf = [0u8; 256];
        handle.read_exact_at(0, &mut buf).await.unwrap();
        let original = buf;

        handle.write_at(0, &[0xAA; 256]).await.unwrap();

        let mut after = [0u8; 256];
        handle.read_exact_at(0, &mut after).await.unwrap();
        assert_ne!(after, original, "the window served bytes that had changed");
        assert_eq!(after, [0xAA; 256]);
    }

    #[tokio::test]
    async fn an_empty_read_is_answered_without_touching_the_handle() {
        let handle = ReadAhead::new(CountingHandle::new(payload(1024)));
        let mut buf = [0u8; 0];
        assert_eq!(handle.read_at(0, &mut buf).await.unwrap(), 0);
        assert_eq!(handle.inner().reads(), 0);
    }

    #[tokio::test]
    async fn len_passes_through_to_the_handle() {
        let handle = ReadAhead::new(CountingHandle::new(payload(1234)));
        assert_eq!(handle.len(), 1234);
        assert!(!handle.is_empty());
    }
}
