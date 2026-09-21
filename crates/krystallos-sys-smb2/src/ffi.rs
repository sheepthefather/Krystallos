//! Raw FFI declarations for the vendored libsmb2.
//!
//! # Why these are written by hand
//!
//! Generated bindings would be the default choice, and `bindgen` is what the
//! original plan called for. It was dropped for two reasons:
//!
//! 1. **It needs libclang on every build machine.** `libclang` is not present
//!    on the development machine, and the copy bundled with the Android NDK
//!    cannot serve the host-target build — parsing Windows SDK headers needs a
//!    clang that knows about them.
//! 2. **The surface we need is tiny.** Around thirty functions out of the
//!    library's ~170, plus three structs. Generation was buying completeness we
//!    have no use for, at the cost of a heavy toolchain dependency on every
//!    platform we cross-compile to.
//!
//! # What is declared, and what is not
//!
//! Only the **synchronous** API, because that is what `krystallos-smb` drives
//! (see that crate's module docs for why). libsmb2 also offers an asynchronous
//! API — `smb2_*_async`, plus `smb2_get_fd` / `smb2_which_events` /
//! `smb2_service` for hand-driving the socket — and those declarations are
//! deliberately absent rather than merely unused. An `extern` declaration is a
//! claim about an ABI that nothing in the build verifies, so a declaration with
//! no caller is a liability that can rot silently. They are at
//! `libsmb2.h:563-1100` and `:1560-1620` when they are needed.
//!
//! # Keeping it honest
//!
//! Hand-written declarations can drift from the header. Two things guard
//! against that:
//!
//! - **Layout assertions.** Every `#[repr(C)]` struct below is checked at
//!   compile time against the size, alignment and field offsets transcribed
//!   from the header. A field added, reordered or resized upstream fails the
//!   build rather than silently reading the wrong bytes.
//! - **Integration tests.** `krystallos-smb` exercises `stat` and directory
//!   listing against a real server, so an offset that is wrong but still
//!   compiles shows up as implausible file sizes rather than as a subtly wrong
//!   answer.
//!
//! Every declaration was transcribed from
//! `vendor/libsmb2/include/smb2/libsmb2.h` at submodule commit `557e837`.
//! **When the submodule moves, re-check this file** — the line numbers in the
//! comments say what to compare against.
//!
//! # Safety
//!
//! This is the only module in the workspace containing `unsafe`. Everything
//! above it goes through the safe wrapper in `krystallos-smb`.

#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int};

/// Opaque session handle. Owned by [`smb2_init_context`].
#[repr(C)]
pub struct smb2_context {
    _private: [u8; 0],
}

/// Opaque open-file handle. Released by [`smb2_close`].
#[repr(C)]
pub struct smb2fh {
    _private: [u8; 0],
}

/// Opaque directory handle. Released by [`smb2_closedir`].
#[repr(C)]
pub struct smb2dir {
    _private: [u8; 0],
}

/// File metadata as libsmb2 reports it.
///
/// Transcribed from `libsmb2.h:86-111`. **Deliberately not exposed upward** —
/// `krystallos-core` has its own portable `Metadata`, and the protocol-specific
/// fields here (`smb2_attributes`, `smb2_reparse_tag`) are exactly the kind of
/// thing that must not leak past the backend.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
#[allow(non_snake_case)]
pub struct smb2_stat_64 {
    pub smb2_type: u32,
    pub smb2_nlink: u32,
    pub smb2_ino: u64,
    pub smb2_size: u64,
    pub smb2_atime: u64,
    pub smb2_atime_nsec: u64,
    pub smb2_mtime: u64,
    pub smb2_mtime_nsec: u64,
    pub smb2_ctime: u64,
    pub smb2_ctime_nsec: u64,
    pub smb2_btime: u64,
    pub smb2_btime_nsec: u64,
    pub smb2_attributes: u32,
    pub smb2_reparse_tag: u32,
}

/// A directory entry: a borrowed name plus the metadata that came with it.
///
/// Transcribed from `libsmb2.h:127-130`. `name` points into memory owned by the
/// `smb2dir` and is only valid until the next [`smb2_readdir`] or
/// [`smb2_closedir`] on that handle — copy it out before doing anything else.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(non_snake_case)]
pub struct smb2dirent {
    pub name: *const c_char,
    pub st: smb2_stat_64,
}

// ---------------------------------------------------------------------------
// Layout assertions
// ---------------------------------------------------------------------------
//
// These are the point of hand-writing the declarations. If libsmb2 changes a
// struct and this file is not updated, the build stops here.
//
// The expected numbers are computed from the field list in the header, not
// copied from a run: `smb2_stat_64` is twelve 64-bit fields preceded by two
// 32-bit ones and followed by two more, so it is 96 bytes with no padding. A
// pointer precedes it in `smb2dirent`, giving 104 bytes on both 32- and 64-bit
// targets — the pointer is 4 bytes on 32-bit, but `smb2_stat_64` needs 8-byte
// alignment, so the same 4 bytes of padding appear either way.

const _: () = {
    use std::mem::{align_of, offset_of, size_of};

    assert!(size_of::<smb2_stat_64>() == 96, "smb2_stat_64 size changed");
    assert!(align_of::<smb2_stat_64>() == 8, "smb2_stat_64 alignment changed");
    assert!(offset_of!(smb2_stat_64, smb2_type) == 0);
    assert!(offset_of!(smb2_stat_64, smb2_nlink) == 4);
    assert!(offset_of!(smb2_stat_64, smb2_ino) == 8);
    assert!(offset_of!(smb2_stat_64, smb2_size) == 16);
    assert!(offset_of!(smb2_stat_64, smb2_mtime) == 40);
    assert!(offset_of!(smb2_stat_64, smb2_btime_nsec) == 80);
    assert!(offset_of!(smb2_stat_64, smb2_attributes) == 88);
    assert!(offset_of!(smb2_stat_64, smb2_reparse_tag) == 92);

    assert!(size_of::<smb2dirent>() == 104, "smb2dirent size changed");
    assert!(align_of::<smb2dirent>() == 8, "smb2dirent alignment changed");
    assert!(offset_of!(smb2dirent, name) == 0);
    assert!(offset_of!(smb2dirent, st) == 8);
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// `smb2_stat_64::smb2_type` values (`libsmb2.h:78-84`).
pub const SMB2_TYPE_FILE: u32 = 0x0000_0000;
pub const SMB2_TYPE_DIRECTORY: u32 = 0x0001;
pub const SMB2_TYPE_LINK: u32 = 0x0002;
pub const SMB2_TYPE_FIFO: u32 = 0x0003;
pub const SMB2_TYPE_CHARDEV: u32 = 0x0004;
pub const SMB2_TYPE_BLOCKDEV: u32 = 0x0005;
pub const SMB2_TYPE_SOCKET: u32 = 0x0006;

/// `smb2_stat_64::smb2_attributes` bits (`smb2.h:272-286`).
pub const SMB2_FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
pub const SMB2_FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const SMB2_FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Open flags for [`smb2_open`], taken from the target platform's `fcntl.h`.
///
/// libsmb2 does not define these itself — it includes the platform header, or
/// has `lib/compat.h` fill them in — and it interprets whatever number it
/// receives using the values it was compiled against. So they have to be the
/// *target's* values, not a fixed set: `O_CREAT` is `0x100` on MSVC and `0o100`
/// on Linux, and passing one platform's number to the other silently asks for
/// something else. On Unix these come from `libc` for exactly that reason.
pub mod open_flags {
    #[cfg(unix)]
    pub use libc::{O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};

    #[cfg(windows)]
    mod msvc {
        use std::os::raw::c_int;

        // Values from the MSVC `<fcntl.h>`.
        pub const O_RDONLY: c_int = 0x0000;
        pub const O_WRONLY: c_int = 0x0001;
        pub const O_RDWR: c_int = 0x0002;
        pub const O_CREAT: c_int = 0x0100;
        pub const O_TRUNC: c_int = 0x0200;
        pub const O_EXCL: c_int = 0x0400;
    }

    #[cfg(windows)]
    pub use msvc::{O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
}

// Edition 2024 requires the `unsafe` keyword on `extern` blocks: declaring a
// foreign function is itself an unchecked assertion that the signature matches
// what the library actually exports.
unsafe extern "C" {
    // -- lifecycle ---------------------------------------------------------

    /// Allocate a session context. Returns null on allocation failure.
    pub fn smb2_init_context() -> *mut smb2_context;

    /// Release a session context. Consumes the pointer.
    pub fn smb2_destroy_context(smb2: *mut smb2_context);

    // -- configuration (libsmb2.h:400-560) ---------------------------------

    pub fn smb2_set_user(smb2: *mut smb2_context, user: *const c_char);
    pub fn smb2_set_password(smb2: *mut smb2_context, password: *const c_char);
    pub fn smb2_set_domain(smb2: *mut smb2_context, domain: *const c_char);

    /// Request SMB3 encryption. Non-zero enables it. Takes effect during the
    /// handshake, so it must be set before connecting.
    pub fn smb2_set_seal(smb2: *mut smb2_context, val: c_int);

    /// Require signing. Non-zero enables it.
    pub fn smb2_set_sign(smb2: *mut smb2_context, val: c_int);

    /// Per-operation timeout in seconds, after which a blocking call returns
    /// with an error rather than waiting indefinitely.
    pub fn smb2_set_timeout(smb2: *mut smb2_context, seconds: c_int);

    // -- connection (libsmb2.h:560) ----------------------------------------

    /// Connect to a share. `user` may be null for anonymous.
    ///
    /// Blocks until the tree connect completes or the timeout expires.
    pub fn smb2_connect_share(
        smb2: *mut smb2_context,
        server: *const c_char,
        share: *const c_char,
        user: *const c_char,
    ) -> c_int;

    pub fn smb2_disconnect_share(smb2: *mut smb2_context) -> c_int;

    // -- directory (libsmb2.h:742, 775, 786) -------------------------------

    /// Open a directory. Returns null on failure.
    ///
    /// **This fetches the entire listing before returning** — the caller gets a
    /// buffer, not a cursor over the network. A directory with a very large
    /// number of entries materialises all of them at once.
    pub fn smb2_opendir(smb2: *mut smb2_context, path: *const c_char) -> *mut smb2dir;

    /// Next entry, or null at end of directory.
    ///
    /// Never blocks — it walks the buffer `smb2_opendir` filled, which is why
    /// libsmb2 provides no asynchronous version. The returned pointer is owned
    /// by the `smb2dir` and is invalidated by the next call.
    pub fn smb2_readdir(smb2: *mut smb2_context, dir: *mut smb2dir) -> *mut smb2dirent;

    /// Free a directory handle. Pure memory release; no network traffic.
    pub fn smb2_closedir(smb2: *mut smb2_context, dir: *mut smb2dir);

    // -- metadata (libsmb2.h:637) ------------------------------------------

    pub fn smb2_stat(
        smb2: *mut smb2_context,
        path: *const c_char,
        st: *mut smb2_stat_64,
    ) -> c_int;

    /// Metadata for an already-open file, keyed by handle rather than path.
    ///
    /// This is how the size is obtained after opening: the create response
    /// carries it, but the synchronous API does not surface it, so the size has
    /// to be queried separately.
    pub fn smb2_fstat(
        smb2: *mut smb2_context,
        fh: *mut smb2fh,
        st: *mut smb2_stat_64,
    ) -> c_int;

    // -- file I/O (libsmb2.h:790-1010) -------------------------------------

    /// `flags` are POSIX `O_*` values.
    pub fn smb2_open(smb2: *mut smb2_context, path: *const c_char, flags: c_int)
        -> *mut smb2fh;

    pub fn smb2_close(smb2: *mut smb2_context, fh: *mut smb2fh) -> c_int;

    /// Positioned read.
    ///
    /// **`offset` is the last parameter**, unlike POSIX `pread(2)` — which is
    /// the easiest thing in this file to get wrong, and it fails by silently
    /// reading the wrong part of the file rather than by failing to compile.
    ///
    /// Returns the number of bytes read, or a negative `-errno`. Requests above
    /// [`smb2_get_max_read_size`] are not split for you.
    pub fn smb2_pread(
        smb2: *mut smb2_context,
        fh: *mut smb2fh,
        buf: *mut u8,
        count: u32,
        offset: u64,
    ) -> c_int;

    /// Positioned write. Same parameter-order caveat as [`smb2_pread`].
    pub fn smb2_pwrite(
        smb2: *mut smb2_context,
        fh: *mut smb2fh,
        buf: *const u8,
        count: u32,
        offset: u64,
    ) -> c_int;

    pub fn smb2_fsync(smb2: *mut smb2_context, fh: *mut smb2fh) -> c_int;

    pub fn smb2_ftruncate(smb2: *mut smb2_context, fh: *mut smb2fh, length: u64) -> c_int;

    /// Largest single read the negotiated dialect permits. Reads must be split
    /// to this size; it varies with the server and with SMB 3.1.1's
    /// multi-credit grants.
    pub fn smb2_get_max_read_size(smb2: *mut smb2_context) -> u32;

    pub fn smb2_get_max_write_size(smb2: *mut smb2_context) -> u32;

    // -- namespace operations (libsmb2.h:1150-1240) ------------------------

    pub fn smb2_unlink(smb2: *mut smb2_context, path: *const c_char) -> c_int;
    pub fn smb2_rename(
        smb2: *mut smb2_context,
        oldpath: *const c_char,
        newpath: *const c_char,
    ) -> c_int;
    pub fn smb2_mkdir(smb2: *mut smb2_context, path: *const c_char) -> c_int;
    pub fn smb2_rmdir(smb2: *mut smb2_context, path: *const c_char) -> c_int;

    // -- errors (libsmb2.h:1700-1730) --------------------------------------

    /// Description of the last error on this context.
    ///
    /// The pointer is owned by the context and is invalidated by the next call
    /// on it — copy it out immediately, on the same thread.
    pub fn smb2_get_error(smb2: *mut smb2_context) -> *const c_char;

    /// The raw NT status of the last failed operation, e.g. `0xC0000034` for
    /// `STATUS_OBJECT_NAME_NOT_FOUND`.
    pub fn smb2_get_nterror(smb2: *mut smb2_context) -> c_int;

    /// Translate an NT status into a human-readable string. Returns a pointer
    /// to static storage; never freed.
    pub fn nterror_to_str(status: u32) -> *const c_char;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, size_of};

    #[test]
    fn struct_layout_matches_the_header() {
        // Belt and braces: the const assertions above already fail the build,
        // but a test names the problem in the output when it does.
        assert_eq!(size_of::<smb2_stat_64>(), 96);
        assert_eq!(align_of::<smb2_stat_64>(), 8);
        assert_eq!(size_of::<smb2dirent>(), 104);
    }

    #[test]
    fn type_and_attribute_constants_do_not_overlap() {
        // A transposed constant would make a directory report as a file, or a
        // writable file as read-only, without anything failing to compile.
        assert_eq!(SMB2_TYPE_FILE, 0);
        assert_eq!(SMB2_TYPE_DIRECTORY, 1);
        assert_eq!(SMB2_TYPE_LINK, 2);
        assert_eq!(SMB2_FILE_ATTRIBUTE_READONLY, 0x1);
        assert_eq!(SMB2_FILE_ATTRIBUTE_DIRECTORY, 0x10);
        assert_eq!(SMB2_FILE_ATTRIBUTE_REPARSE_POINT, 0x400);
    }
}
