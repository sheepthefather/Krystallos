use crate::backend::LocalBackend;
use async_trait::async_trait;
use krystallos_core::{
    BackendDriver, ConnectionOptions, Credentials, Endpoint, Error, Result, StorageBackend,
};
use std::path::{Path, PathBuf};

/// Opens [`LocalBackend`] sessions for `file://` endpoints.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalDriver;

impl LocalDriver {
    pub fn new() -> Self {
        LocalDriver
    }
}

/// Whether `s` looks like a URI path holding a Windows drive, e.g. `/D:/media`.
fn looks_like_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':'
}

/// Turn the part of a `file://` URI after `://` into a filesystem path.
///
/// # Supported forms
///
/// | URI | Path |
/// |---|---|
/// | `file:///D:/media` | `D:\media` (Windows) |
/// | `file:///home/u/media` | `/home/u/media` |
/// | `file:////server/share` | `\\server\share` (UNC, four slashes) |
///
/// # No percent-decoding
///
/// The path is taken literally. File URIs make `%` ambiguous — `file:///D:/100%
/// done` has to be read as a literal `%`, while `file:///D:/a%20b` arguably
/// encodes a space — and guessing wrong corrupts real paths. A literal `%` in a
/// directory name is common on Windows; an encoded space is not. So: no
/// decoding, and spaces are written as spaces.
fn parse_root(rest: &str) -> Result<PathBuf> {
    if rest.is_empty() {
        return Err(Error::InvalidPath {
            path: "file://".to_string(),
            reason: "a `file://` endpoint must name a directory, e.g. `file:///D:/media`"
                .to_string(),
        });
    }

    // `file:///D:/media` leaves `rest` as `/D:/media`. That leading slash is
    // the URI's path root; on Windows a drive letter follows it, and keeping
    // the slash would produce a path rooted at `\D:` on the current drive
    // rather than at `D:`.
    let path = if cfg!(windows) && looks_like_drive_path(rest) {
        &rest[1..]
    } else {
        rest
    };

    Ok(PathBuf::from(path))
}

/// The inverse of [`parse_root`]: render a canonical root as a `file://` URI.
pub(crate) fn uri_for(root: &Path) -> String {
    let s = root.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        // Already absolute in URI terms: `/home/u` -> `file:///home/u`
        format!("file://{s}")
    } else {
        // A Windows drive: `D:/media` -> `file:///D:/media`
        format!("file:///{s}")
    }
}

#[async_trait]
impl BackendDriver for LocalDriver {
    fn scheme(&self) -> &'static str {
        "file"
    }

    fn description(&self) -> &'static str {
        "Local filesystem"
    }

    async fn connect(
        &self,
        endpoint: &Endpoint,
        _credentials: &Credentials,
        _options: &ConnectionOptions,
    ) -> Result<Box<dyn StorageBackend>> {
        let requested = parse_root(endpoint.authority_and_path())?;

        // Canonicalize up front so that every later operation is relative to a
        // single, stable, absolute root. Doing it here also means a typo in the
        // endpoint fails at connect time rather than on the first listing.
        let canonical = std::fs::canonicalize(&requested).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::NotFound {
                path: requested.display().to_string(),
            },
            _ => Error::Io(e),
        })?;

        let meta = std::fs::metadata(&canonical).map_err(Error::Io)?;
        if !meta.is_dir() {
            return Err(Error::NotADirectory {
                path: canonical.display().to_string(),
            });
        }

        Ok(Box::new(LocalBackend::new(canonical)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use krystallos_core::VfsPath;

    #[test]
    fn rejects_endpoints_with_no_path() {
        let err = parse_root("").unwrap_err();
        match err {
            Error::InvalidPath { reason, .. } => assert!(reason.contains("must name a directory")),
            other => panic!("expected InvalidPath, got {other:?}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn strips_the_uri_slash_before_a_windows_drive() {
        assert_eq!(parse_root("/D:/media").unwrap(), PathBuf::from("D:/media"));
        assert_eq!(parse_root("/c:/x").unwrap(), PathBuf::from("c:/x"));
    }

    #[test]
    fn a_path_that_is_not_a_drive_keeps_its_leading_slash() {
        assert_eq!(parse_root("/home/u").unwrap(), PathBuf::from("/home/u"));
        // Too short to be a drive, and not letter-colon shaped.
        assert_eq!(parse_root("/ab").unwrap(), PathBuf::from("/ab"));
        assert_eq!(parse_root("/1:/x").unwrap(), PathBuf::from("/1:/x"));
    }

    #[test]
    fn four_slashes_yield_a_unc_style_path() {
        // `file:////server/share` -> rest is `//server/share`.
        let p = parse_root("//server/share").unwrap();
        assert_eq!(p.to_string_lossy().replace('\\', "/"), "//server/share");
    }

    #[cfg(windows)]
    #[test]
    fn uri_for_renders_a_drive_root() {
        assert_eq!(uri_for(Path::new("D:\\media")), "file:///D:/media");
    }

    #[test]
    fn uri_for_renders_a_posix_root() {
        assert_eq!(uri_for(Path::new("/home/u")), "file:///home/u");
    }

    #[test]
    fn drive_detection_boundaries() {
        assert!(looks_like_drive_path("/D:/x"));
        // A bare `D:` is drive-relative on Windows (the current directory on
        // that drive); it is still drive-shaped, and `canonicalize` resolves it.
        assert!(looks_like_drive_path("/D:"));
        assert!(!looks_like_drive_path("/D"), "the colon is what makes it a drive");
        assert!(!looks_like_drive_path("D:/x"), "must start with a slash");
        assert!(!looks_like_drive_path("/DD:/x"), "one letter only");
        assert!(!looks_like_drive_path("/1:/x"), "the letter must be alphabetic");
    }

    #[tokio::test]
    async fn connect_through_the_driver_yields_a_usable_backend() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"content").unwrap();

        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let uri = uri_for(&canonical);
        let endpoint = Endpoint::parse(&uri).unwrap();

        let driver = LocalDriver::new();
        let backend = driver
            .connect(&endpoint, &Credentials::anonymous(), &ConnectionOptions::new())
            .await
            .unwrap();

        let entries = backend.list(&VfsPath::root()).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].metadata.len, 7);
    }

    #[tokio::test]
    async fn connecting_to_a_missing_directory_names_it() {
        let driver = LocalDriver::new();
        let endpoint = Endpoint::parse("file:///definitely/not/here/at/all").unwrap();
        let err = driver
            .connect(&endpoint, &Credentials::anonymous(), &ConnectionOptions::new())
            .await
            .err()
            .expect("connecting to a non-existent directory must fail");
        assert!(matches!(err, Error::NotFound { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn connecting_to_a_file_rather_than_a_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"x").unwrap();

        let uri = uri_for(&std::fs::canonicalize(&file).unwrap());
        let endpoint = Endpoint::parse(&uri).unwrap();
        let err = LocalDriver::new()
            .connect(&endpoint, &Credentials::anonymous(), &ConnectionOptions::new())
            .await
            .err()
            .expect("a file is not a valid root");
        assert!(matches!(err, Error::NotADirectory { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn credentials_are_ignored_by_the_local_backend() {
        let dir = tempfile::tempdir().unwrap();
        let uri = uri_for(&std::fs::canonicalize(dir.path()).unwrap());
        let endpoint = Endpoint::parse(&uri).unwrap();

        // A local filesystem has no notion of a login, so credentials must be
        // accepted and ignored rather than causing a confusing failure.
        let backend = LocalDriver::new()
            .connect(
                &endpoint,
                &Credentials::user_password("someone", "secret"),
                &ConnectionOptions::new(),
            )
            .await
            .unwrap();
        backend.shutdown().await.unwrap();
    }
}
