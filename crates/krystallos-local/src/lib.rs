//! Local filesystem backend.
//!
//! This backend exists for two reasons, neither of which is "so the player can
//! read local files":
//!
//! 1. **It is the only place where "remote storage that behaves like a local
//!    filesystem" is literally true.** Any concept in `krystallos-core` that
//!    only makes sense for SMB will be impossible to implement here, so this
//!    backend is what keeps the abstraction honest. If a `share`, an NTSTATUS
//!    code, or a wire-format attribute ever appears in the core crate, this
//!    crate cannot be written.
//!
//! 2. **It is the differential-test baseline.** The same file read through
//!    this backend and through the SMB backend must produce identical bytes.
//!    That single comparison checks both that the abstraction leaks nothing and
//!    that the SMB implementation is correct — far stronger than testing SMB
//!    against itself.
//!
//! # Not a production hot path
//!
//! Positioned reads and writes go straight to blocking syscalls rather than
//! through `spawn_blocking`. The async trait takes `&mut [u8]`, which cannot be
//! moved into a `'static` blocking task without copying the buffer — a copy
//! that would cost more than the syscall it avoids. On a local file a
//! positioned read completes in microseconds, so the executor stall is
//! negligible. The network backends are where blocking would actually hurt, and
//! they are asynchronous for real.

#![forbid(unsafe_code)]

mod backend;
mod driver;

pub use backend::LocalBackend;
pub use driver::{uri_for, LocalDriver};
