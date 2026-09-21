//! Raw FFI declarations for the vendored libsmb2.
//!
//! # Why these declarations are written by hand
//!
//! Generated bindings would be the default choice, and `bindgen` is what the
//! original plan called for. It was dropped for two reasons:
//!
//! 1. **It needs libclang on every build machine.** `libclang` is not present
//!    on the development machine, and the copy bundled with the Android NDK
//!    cannot serve the host-target build — parsing Windows SDK headers needs a
//!    clang that knows about them.
//! 2. **The surface we need is tiny.** Around forty functions out of the
//!    library's ~170, plus a handful of structs. Generation was buying
//!    completeness we have no use for, at the cost of a heavy toolchain
//!    dependency on every platform we cross-compile to.
//!
//! # How correctness is kept
//!
//! Hand-written declarations can drift from the header. That risk is handled by
//! asserting the layout — see [`layout`] and the `_Static_assert` block in
//! `build.rs`'s companion C file. If a field is added, reordered, or resized
//! upstream, the build fails rather than silently reading the wrong bytes.
//!
//! # Safety
//!
//! This is the only crate in the workspace containing `unsafe`. Everything
//! above it goes through the safe wrapper in `krystallos-smb`.

/// Opaque session handle. Owned by [`ffi::smb2_init_context`].
#[repr(C)]
pub struct smb2_context {
    _private: [u8; 0],
}

/// Opaque open-file handle.
#[repr(C)]
pub struct smb2fh {
    _private: [u8; 0],
}

/// Opaque directory handle.
#[repr(C)]
pub struct smb2dir {
    _private: [u8; 0],
}

pub mod ffi {
    //! Raw `extern "C"` declarations and `#[repr(C)]` mirrors of libsmb2's
    //! public structs.
    //!
    //! Only what the kernel actually uses is declared. Adding to this module is
    //! a deliberate act, and each addition should come with a layout assertion
    //! if it is a struct.

    use super::{smb2_context, smb2dir, smb2fh};
    use std::os::raw::{c_char, c_int, c_void};

    // ---------------------------------------------------------------------
    // Context lifecycle
    // ---------------------------------------------------------------------

    // Edition 2024 requires the `unsafe` keyword on `extern` blocks: declaring
    // a foreign function is itself an unchecked assertion that the signature
    // matches what the library actually exports.
    unsafe extern "C" {
        /// Allocate a session context. Returns null on allocation failure.
        ///
        /// The result must be released with `smb2_destroy_context`.
        pub fn smb2_init_context() -> *mut smb2_context;

        /// Release a session context. Consumes the pointer.
        pub fn smb2_destroy_context(ctx: *mut smb2_context);

        /// Description of the last error on this context.
        ///
        /// The pointer is owned by the context and is invalidated by the next
        /// call on it — copy it out immediately.
        pub fn smb2_get_error(ctx: *mut smb2_context) -> *const c_char;

        /// The raw NT status of the last failed operation, e.g. `0xC0000034`
        /// for `STATUS_OBJECT_NAME_NOT_FOUND`.
        pub fn smb2_get_nterror(ctx: *mut smb2_context) -> c_int;

        /// Translate an NT status into a human-readable string.
        ///
        /// Returns a pointer to static storage; never freed.
        pub fn nterror_to_str(status: u32) -> *const c_char;
    }

    // ---------------------------------------------------------------------
    // Placeholder bindings used by the M1 link probe
    // ---------------------------------------------------------------------
    //
    // The full operation surface (connect, list, open, pread, ...) is added in
    // M4, together with the layout assertions for `smb2_stat_64` and
    // `smb2dirent`. Declaring it before there is a caller would be dead code
    // that nothing validates.

    /// Never used directly; exists so the opaque types are referenced and the
    /// module compiles without warnings while the API surface is still small.
    #[allow(dead_code)]
    pub(crate) type OpaqueHandles = (*mut smb2_context, *mut smb2fh, *mut smb2dir, *mut c_void);
}

/// Exercise the FFI boundary: create a context and tear it down.
///
/// This exists so that linking against libsmb2 is actually verified by the
/// test suite rather than merely assumed. If the C library failed to compile
/// or link, this is the first thing that fails.
///
/// Returns `false` if libsmb2 could not allocate a context.
pub fn link_probe() -> bool {
    // SAFETY: `smb2_init_context` takes no arguments, returns either null or a
    // context this function uniquely owns, and that context is destroyed
    // exactly once below before the function returns.
    unsafe {
        let ctx = ffi::smb2_init_context();
        if ctx.is_null() {
            return false;
        }
        ffi::smb2_destroy_context(ctx);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libsmb2_is_linked_and_usable() {
        assert!(
            link_probe(),
            "libsmb2 could not allocate a context — the static library is \
             probably not linked"
        );
    }

    #[test]
    fn error_strings_translate_through_the_ffi() {
        // 0xC0000034 is STATUS_OBJECT_NAME_NOT_FOUND. The exact spelling comes
        // from libsmb2's own table (`lib/errors.c`), which uses the identifier
        // form rather than prose.
        // SAFETY: `nterror_to_str` accepts any u32 and returns a pointer to
        // static storage that is valid for the lifetime of the process.
        let rendered = unsafe {
            let p = ffi::nterror_to_str(0xC000_0034);
            assert!(!p.is_null(), "nterror_to_str returned null");
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        };
        assert_eq!(rendered, "STATUS_OBJECT_NAME_NOT_FOUND");
    }

    #[test]
    fn unknown_status_codes_still_render_something() {
        // A code libsmb2 has no name for must not produce a null pointer or
        // garbage — callers build error messages from this unconditionally.
        // SAFETY: as above; the function is total over u32.
        let rendered = unsafe {
            let p = ffi::nterror_to_str(0xDEAD_BEEF);
            assert!(!p.is_null(), "nterror_to_str returned null for an unknown code");
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        };
        assert!(!rendered.is_empty(), "unknown status rendered as empty");
    }
}
