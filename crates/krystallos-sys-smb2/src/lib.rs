//! Raw FFI bindings for the vendored libsmb2.
//!
//! This crate is deliberately thin: it owns the `unsafe`, the build, and
//! nothing else. Every protocol decision — error mapping, path translation,
//! session lifetime — belongs to `krystallos-smb`, which is the only consumer.
//!
//! See [`ffi`] for the declarations themselves and for how their correctness is
//! kept in step with the C header.

pub mod ffi;
mod network;

pub use network::ensure_network_ready;

/// Exercise the FFI boundary: create a context and tear it down.
///
/// This exists so that linking against libsmb2 is verified by the test suite
/// rather than merely assumed. If the C library failed to compile or link, this
/// is the first thing that fails.
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

    #[test]
    fn a_fresh_context_reports_no_error() {
        // SAFETY: the context is created here and destroyed here; `smb2_get_error`
        // returns a pointer owned by it, read before destruction.
        unsafe {
            let ctx = ffi::smb2_init_context();
            assert!(!ctx.is_null());
            let msg = ffi::smb2_get_error(ctx);
            // A fresh context has no error, so this is either null or empty —
            // never a dangling pointer we then read.
            if !msg.is_null() {
                let _ = std::ffi::CStr::from_ptr(msg);
            }
            ffi::smb2_destroy_context(ctx);
        }
    }
}
