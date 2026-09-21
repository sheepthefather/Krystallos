//! Generates the Kotlin bindings from the compiled library.
//!
//! UniFFI has no Gradle plugin, so binding generation is a separate step that
//! reads the finished `.so` and emits a self-contained `.kt` file. It is driven
//! from here rather than from a globally installed `uniffi-bindgen` so that the
//! generator's version is pinned by `Cargo.lock` and cannot drift from the
//! `uniffi` version the library was built with. A mismatched pair produces
//! bindings that compile and then fail at run time, which is a bad afternoon.
//!
//! Usage, from the workspace root:
//!
//! ```text
//! cargo run -p krystallos-ffi --bin uniffi-bindgen -- \
//!     generate --library <path to .so> --language kotlin --out-dir <dir>
//! ```
//!
//! With `--features uniffi/cli` the subcommand parsing comes from UniFFI itself,
//! so the arguments are exactly what its documentation describes.

fn main() {
    uniffi::uniffi_bindgen_main()
}
