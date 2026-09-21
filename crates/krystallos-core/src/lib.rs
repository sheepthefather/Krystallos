//! Protocol-neutral storage abstraction for the Krystallos kernel.
//!
//! This crate defines the contract that every storage backend implements. It is
//! deliberately free of any protocol-specific concept: you will not find shares,
//! NTSTATUS codes, or wire-format flags here. Backends translate their own model
//! into the types below, and callers (the CLI, the FFI facade, a future media
//! player) work only against this contract.
//!
//! # Shape of the API
//!
//! The goal is that callers can treat remote storage *like* a local filesystem.
//! The API therefore looks local — `open`, `read_at`, `write_at`, `list`,
//! `stat` — but three things about remote storage genuinely differ from local
//! storage, and the API is shaped to admit that rather than hide it:
//!
//! 1. **Latency differs by three to four orders of magnitude.** Every operation
//!    is `async`. A blocking `std::fs`-style signature would be a lie.
//! 2. **Per-entry `stat` is an N+1 round-trip disaster.** [`StorageBackend::list`]
//!    returns [`Entry`] values that already carry their [`Metadata`], because
//!    most protocols hand both back from a single directory enumeration.
//! 3. **Network faults are routine, not exceptional.** [`Error::ConnectionLost`]
//!    is distinct from [`Error::Io`] so that callers can decide whether to
//!    re-establish a session or surface a per-file failure.
//!
//! # Capability differences are declared, not discovered
//!
//! Protocols are not equally capable. FTP has no positioned read at all;
//! WebDAV has no standard partial write. [`StorageBackend::capabilities`]
//! declares these gaps up front so a caller can adapt — rather than finding out
//! by having an operation fail halfway.

#![forbid(unsafe_code)]

mod backend;
mod capabilities;
mod entry;
mod error;
mod path;
mod registry;

pub use backend::{FileHandle, OpenMode, StorageBackend};
pub use capabilities::Capabilities;
pub use entry::{Entry, EntryKind, Metadata};
pub use error::{Error, Result};
pub use path::VfsPath;
pub use registry::{BackendDriver, BackendRegistry, Credentials, Endpoint};
