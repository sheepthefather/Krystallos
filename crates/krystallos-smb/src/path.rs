use krystallos_core::{Error, Result, VfsPath};
use std::ffi::CString;

/// Convert a [`VfsPath`] into the form libsmb2 expects.
///
/// Two details, both taken from the library rather than guessed:
///
/// - **Paths are relative to the share root and carry no leading separator.**
///   `lib/init.c:276-287` parses `smb2://server/share/dir/file` into share
///   `share` and path `dir/file`.
/// - **The share root is the empty string.** `lib/smb2-cmd-create.c:86` treats a
///   null or empty name as "no name", which is how SMB addresses the root of a
///   tree.
///
/// The `/` separator is kept as-is: libsmb2 converts UTF-8 to UTF-16 without
/// rewriting separators, and SMB servers accept forward slashes.
pub(crate) fn to_smb_path(path: &VfsPath) -> String {
    path.segments().collect::<Vec<_>>().join("/")
}

/// A path as a NUL-terminated C string.
///
/// Fails only on an interior NUL byte, which cannot occur in a path libsmb2
/// could send anyway — but silently truncating there would address a different
/// file, so it is an error rather than something to paper over.
pub(crate) fn cstring(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::InvalidPath {
        path: s.replace('\0', "\\0"),
        reason: "contains an interior NUL byte".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> VfsPath {
        VfsPath::new(s).unwrap()
    }

    #[test]
    fn the_root_becomes_the_empty_string() {
        assert_eq!(to_smb_path(&VfsPath::root()), "");
    }

    #[test]
    fn nested_paths_lose_the_leading_separator() {
        assert_eq!(to_smb_path(&p("/a")), "a");
        assert_eq!(to_smb_path(&p("/a/b/c")), "a/b/c");
        assert_eq!(to_smb_path(&p("a/b")), "a/b");
    }

    #[test]
    fn unicode_names_pass_through_unchanged() {
        // No normalization, no escaping: the backend must send exactly the
        // bytes it was given, or lookups fail on servers that store names in a
        // different normalization form.
        assert_eq!(to_smb_path(&p("/影片/第 1 集.mkv")), "影片/第 1 集.mkv");
    }

    #[test]
    fn cstring_accepts_ordinary_paths() {
        assert_eq!(cstring("a/b").unwrap().to_str().unwrap(), "a/b");
        assert!(cstring("").unwrap().to_str().unwrap().is_empty());
    }

    #[test]
    fn cstring_rejects_an_interior_nul() {
        let err = cstring("a\0b").unwrap_err();
        match err {
            Error::InvalidPath { reason, .. } => assert!(reason.contains("NUL")),
            other => panic!("expected InvalidPath, got {other:?}"),
        }
    }
}
