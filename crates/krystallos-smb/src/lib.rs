//! SMB2/3 backend, built on libsmb2.
//!
//! # Threading model
//!
//! **libsmb2 is not thread-safe.** There are no locks anywhere in its `lib/`
//! sources, and its synchronous API is a `poll(…, 1000)` loop over shared state
//! with no protection. A context therefore belongs to exactly one thread for
//! its whole life, and that is not a guideline — sharing one produces
//! corruption.
//!
//! So every session gets a dedicated OS thread which owns the context. Callers
//! reach it by sending commands over a channel and awaiting a reply; the
//! context itself never leaves the thread and no pointer to it is ever handed
//! out. This is the whole of the concurrency story, and it means the rest of
//! the crate needs no locks at all.
//!
//! # Why blocking calls rather than a hand-driven event loop
//!
//! libsmb2 also offers an asynchronous API plus `smb2_get_fd` /
//! `smb2_which_events` / `smb2_service`, which would let a single session have
//! several operations in flight. That was the original plan. It was set aside
//! after looking at what it costs:
//!
//! The event loop has to learn about new commands while it is blocked in
//! `poll`. Feeding it from another thread means either a wake-up channel bolted
//! into the poll set (a pipe on Unix, a loopback socket on Windows — two more
//! platform-specific pieces to get right), or a short poll timeout, which puts
//! a latency floor under every operation and keeps the CPU waking up several
//! times a second forever. On a phone that last one is a battery cost paid
//! continuously to save a few milliseconds occasionally.
//!
//! Blocking calls have neither problem: the thread parks in `recv` when idle
//! at zero cost and wakes the instant a command arrives. The price is that one
//! session runs one operation at a time — which for a media player reading a
//! single stream is what happens anyway.
//!
//! The public API is identical either way, so if read-ahead (see
//! `krystallos-cache`) later shows that pipelining several reads per session is
//! worth it, the change is confined to this module.

#![forbid(unsafe_op_in_unsafe_fn)]

mod actor;
mod backend;
mod error;
mod file;
mod path;

pub use backend::{SmbBackend, SmbDriver, OPT_SEAL};
pub use file::RemoteFile;

/// Default per-operation timeout, in seconds.
///
/// libsmb2's default is generous; a media player wants to give up on an
/// unreachable share and tell the user rather than appear to hang. Because
/// operations are serialised on the session thread, this is also the worst-case
/// time any queued operation waits behind a stuck one.
pub const DEFAULT_TIMEOUT_SECS: u32 = 20;
