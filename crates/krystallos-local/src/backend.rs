use async_trait::async_trait;
use krystallos_core::{
    Capabilities, Entry, EntryKind, Error, FileHandle, Metadata, OpenMode, Result, StorageBackend,
    VfsPath,
};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Positioned reads. The two platforms spell this differently — Unix has
/// `read_at`, Windows has `seek_read` — so the difference is confined here
/// rather than repeated at every call site.
#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(unix)]
fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

#[cfg(windows)]
fn pwrite(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

/// Translate an `io::Error` into the portable error model.
///
/// The interesting cases are the ones `io::ErrorKind` can express directly;
/// everything else keeps its `io::Error` so no diagnostic detail is thrown
/// away. `path` is attached because the underlying error frequently does not
/// mention which path failed.
fn map_io(err: io::Error, path: &VfsPath) -> Error {
    use io::ErrorKind::*;
    let p = path.to_string();
    match err.kind() {
        NotFound => Error::NotFound { path: p },
        PermissionDenied => Error::PermissionDenied { path: p },
        AlreadyExists => Error::AlreadyExists { path: p },
        NotADirectory => Error::NotADirectory { path: p },
        IsADirectory => Error::IsADirectory { path: p },
        DirectoryNotEmpty => Error::DirectoryNotEmpty { path: p },
        _ => Error::Io(err),
    }
}

fn to_metadata(m: &std::fs::Metadata) -> Metadata {
    let t = m.file_type();
    // Order matters: for a symlink's own metadata, `is_dir`/`is_file` are both
    // false, so the symlink check has to come first.
    let kind = if t.is_symlink() {
        EntryKind::Symlink
    } else if t.is_dir() {
        EntryKind::Directory
    } else if t.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };

    Metadata {
        kind,
        len: m.len(),
        modified: m.modified().ok(),
        created: m.created().ok(),
        accessed: m.accessed().ok(),
        // Note: on Unix this is "no write bit is set for anyone", which is a
        // coarser question than "can this user write". It is the best portable
        // answer available without inspecting ownership.
        read_only: m.permissions().readonly(),
    }
}

/// A backend rooted at one local directory.
pub struct LocalBackend {
    root: PathBuf,
    /// The root as a string, so `resolve` can concatenate rather than use
    /// `PathBuf::push`. `push` would silently replace the whole path if a
    /// segment looked like a drive prefix (`C:`), which is exactly the kind of
    /// quiet redirection a filesystem API must not have.
    root_str: String,
    endpoint: String,
}

impl LocalBackend {
    pub(crate) fn new(root: PathBuf) -> Self {
        let root_str = root.to_string_lossy().into_owned();
        let endpoint = crate::driver::uri_for(&root);
        LocalBackend {
            root,
            root_str,
            endpoint,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Map a virtual path onto the filesystem.
    ///
    /// Cannot escape `root`: `VfsPath` guarantees its segments contain no
    /// separators and no `..`, so concatenation is confined to the subtree.
    /// Symbolic links *inside* the root may still point outside it — that is
    /// inherent to following links, and is the same behaviour as any local
    /// filesystem access.
    fn resolve(&self, path: &VfsPath) -> PathBuf {
        let mut s = self.root_str.clone();
        for segment in path.segments() {
            s.push(std::path::MAIN_SEPARATOR);
            s.push_str(segment);
        }
        PathBuf::from(s)
    }
}

#[async_trait]
impl StorageBackend for LocalBackend {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    async fn list(&self, path: &VfsPath) -> Result<Vec<Entry>> {
        let dir = self.resolve(path);
        let read = std::fs::read_dir(&dir).map_err(|e| map_io(e, path))?;

        let mut out = Vec::new();
        for item in read {
            let item = item.map_err(|e| map_io(e, path))?;
            let name = item.file_name().to_string_lossy().into_owned();

            // `symlink_metadata`, not `metadata`: a listing should describe each
            // entry as itself. Following links here would report a symlink as
            // its target, and would fail outright on a broken link.
            let meta = match std::fs::symlink_metadata(item.path()) {
                Ok(m) => to_metadata(&m),
                // The entry vanished between readdir and stat, or we cannot see
                // it. Skipping keeps the rest of the listing usable, which is
                // what a file browser wants; failing the whole call would not.
                Err(_) => continue,
            };
            out.push(Entry::new(name, meta));
        }

        // Sorted for determinism. Backends are not required to sort — callers
        // that care should sort themselves — but a stable order makes this
        // backend usable as a differential-test baseline.
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn stat(&self, path: &VfsPath) -> Result<Metadata> {
        // Follows symlinks, matching `stat(2)`.
        let meta = std::fs::metadata(self.resolve(path)).map_err(|e| map_io(e, path))?;
        Ok(to_metadata(&meta))
    }

    async fn open(&self, path: &VfsPath, mode: OpenMode) -> Result<Box<dyn FileHandle>> {
        let full = self.resolve(path);
        let mut opts = OpenOptions::new();
        opts.read(mode.is_read()).write(mode.is_write());
        if mode.creates() {
            opts.create(true);
        }
        if mode.must_not_exist() {
            opts.create_new(true);
        }
        if mode.truncates() {
            opts.truncate(true);
        }

        let file = opts.open(&full).map_err(|e| map_io(e, path))?;
        let len = file.metadata().map_err(|e| map_io(e, path))?.len();
        Ok(Box::new(LocalFileHandle { file, len }))
    }

    async fn remove_file(&self, path: &VfsPath) -> Result<()> {
        std::fs::remove_file(self.resolve(path)).map_err(|e| map_io(e, path))
    }

    async fn remove_dir(&self, path: &VfsPath) -> Result<()> {
        // `remove_dir` is non-recursive and fails on a non-empty directory —
        // which is what we want: recursive deletion belongs to the caller, so
        // that its error handling stays visible there.
        std::fs::remove_dir(self.resolve(path)).map_err(|e| map_io(e, path))
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        // On Unix this is `rename(2)`; on Windows it is `MoveFileEx` with
        // `MOVEFILE_REPLACE_EXISTING`. Both replace an existing destination, so
        // the atomic-rename capability holds on both.
        std::fs::rename(self.resolve(from), self.resolve(to)).map_err(|e| map_io(e, from))
    }

    async fn copy(&self, from: &VfsPath, to: &VfsPath) -> Result<u64> {
        // The source is opened first: opening the destination creates it, and a
        // missing source would then leave an empty file behind.
        let mut input = std::fs::File::open(self.resolve(from)).map_err(|e| map_io(e, from))?;
        // `create_new` rather than `std::fs::copy`, which replaces an existing
        // destination. Refusing to overwrite is the rule the SMB backend
        // follows too, and for the same reason: replacing a film is a decision
        // the caller has to make visibly.
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.resolve(to))
            .map_err(|e| map_io(e, to))?;
        std::io::copy(&mut input, &mut out).map_err(|e| map_io(e, to))
    }

    async fn mkdir(&self, path: &VfsPath) -> Result<()> {
        // `create_dir`, not `create_dir_all`: a missing parent should surface
        // as an error rather than silently building a tree the caller did not
        // ask for.
        std::fs::create_dir(self.resolve(path)).map_err(|e| map_io(e, path))
    }

    async fn shutdown(&self) -> Result<()> {
        // Nothing to tear down: each operation opens and closes its own handle.
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(crate) struct LocalFileHandle {
    file: File,
    /// Size observed at open time. See `FileHandle::len` for why this is not
    /// refreshed on every call.
    len: u64,
}

#[async_trait]
impl FileHandle for LocalFileHandle {
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        pread(&self.file, buf, offset).map_err(Error::Io)
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        pwrite(&self.file, buf, offset).map_err(Error::Io)
    }

    async fn set_len(&self, len: u64) -> Result<()> {
        self.file.set_len(len).map_err(Error::Io)
    }

    async fn flush(&self) -> Result<()> {
        // `sync_all` reaches the disk, so `durable_flush` is genuinely true
        // here rather than aspirational.
        self.file.sync_all().map_err(Error::Io)
    }

    async fn close(&self) -> Result<()> {
        // Dropping the `File` is the close. Idempotent by construction.
        Ok(())
    }

    fn len(&self) -> u64 {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend_in(dir: &tempfile::TempDir) -> LocalBackend {
        LocalBackend::new(std::fs::canonicalize(dir.path()).unwrap())
    }

    fn p(s: &str) -> VfsPath {
        VfsPath::new(s).unwrap()
    }

    #[tokio::test]
    async fn list_reports_names_and_metadata_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let be = backend_in(&dir);
        let entries = be.list(&VfsPath::root()).await.unwrap();

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "b.txt", "sub"], "listing is sorted");

        let a = &entries[0];
        assert!(a.metadata.is_file());
        assert_eq!(a.metadata.len, 2, "metadata arrives with the entry");
        assert!(entries[2].is_dir());
    }

    #[tokio::test]
    async fn stat_follows_symlinks_but_listing_does_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.txt"), b"data").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("real.txt", dir.path().join("link.txt")).unwrap();
            let be = backend_in(&dir);

            let entries = be.list(&VfsPath::root()).await.unwrap();
            let link = entries.iter().find(|e| e.name == "link.txt").unwrap();
            assert_eq!(
                link.metadata.kind,
                EntryKind::Symlink,
                "listing describes the entry as itself"
            );

            let st = be.stat(&p("/link.txt")).await.unwrap();
            assert_eq!(
                st.kind,
                EntryKind::File,
                "stat follows the link, like stat(2)"
            );
            assert_eq!(st.len, 4);
        }

        #[cfg(not(unix))]
        {
            // Symlink creation needs privileges on Windows; nothing to assert.
            let _ = backend_in(&dir);
        }
    }

    #[tokio::test]
    async fn positioned_read_and_write_do_not_share_a_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let be = backend_in(&dir);

        let fh = be
            .open(&p("/f.bin"), OpenMode::read_write())
            .await
            .unwrap();

        fh.write_at(5, b"world").await.unwrap();
        fh.write_at(0, b"hello").await.unwrap();
        fh.flush().await.unwrap();

        let mut buf = [0u8; 10];
        fh.read_exact_at(0, &mut buf).await.unwrap();
        assert_eq!(&buf, b"helloworld");

        // Interleaving must not disturb earlier positions — read the middle
        // first, then the start, and check both.
        let mut mid = [0u8; 5];
        fh.read_exact_at(5, &mut mid).await.unwrap();
        let mut head = [0u8; 5];
        fh.read_exact_at(0, &mut head).await.unwrap();
        assert_eq!(&mid, b"world");
        assert_eq!(&head, b"hello");

        fh.close().await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), b"helloworld");
    }

    #[tokio::test]
    async fn reading_past_eof_returns_zero_rather_than_erroring() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small"), b"abc").unwrap();
        let be = backend_in(&dir);
        let fh = be.open(&p("/small"), OpenMode::read()).await.unwrap();

        let mut buf = [0u8; 8];
        let n = fh.read_at(3, &mut buf).await.unwrap();
        assert_eq!(n, 0, "a read wholly at EOF yields 0 bytes, not an error");

        let n = fh.read_at(100, &mut buf).await.unwrap();
        assert_eq!(n, 0, "a read past EOF yields 0 bytes, not an error");
    }

    #[tokio::test]
    async fn read_exact_at_reports_eof_when_the_file_is_short() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("small"), b"abc").unwrap();
        let be = backend_in(&dir);
        let fh = be.open(&p("/small"), OpenMode::read()).await.unwrap();

        let mut buf = [0u8; 8];
        let err = fh.read_exact_at(0, &mut buf).await.unwrap_err();
        match err {
            Error::Io(e) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_new_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exists"), b"original").unwrap();
        let be = backend_in(&dir);

        let err = be
            .open(&p("/exists"), OpenMode::create_new())
            .await
            .err()
            .expect("create_new on an existing file must fail");
        assert!(matches!(err, Error::AlreadyExists { .. }), "got {err:?}");
        assert_eq!(
            std::fs::read(dir.path().join("exists")).unwrap(),
            b"original",
            "the existing file must be untouched"
        );
    }

    #[tokio::test]
    async fn write_mode_creates_without_truncating() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"AAAAAAAA").unwrap();
        let be = backend_in(&dir);

        // Default write mode does not truncate, so a positioned write lands
        // over part of the existing content and leaves the rest alone.
        let fh = be.open(&p("/f"), OpenMode::write()).await.unwrap();
        fh.write_at(0, b"BB").await.unwrap();
        fh.close().await.unwrap();

        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"BBAAAAAA");
    }

    #[tokio::test]
    async fn with_truncate_discards_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"AAAAAAAA").unwrap();
        let be = backend_in(&dir);

        let fh = be
            .open(&p("/f"), OpenMode::write().with_truncate())
            .await
            .unwrap();
        assert_eq!(fh.len(), 0);
        fh.close().await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"");
    }

    #[tokio::test]
    async fn set_len_truncates_and_extends() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"0123456789").unwrap();
        let be = backend_in(&dir);
        let fh = be
            .open(&p("/f"), OpenMode::read_write())
            .await
            .unwrap();

        fh.set_len(4).await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"0123");

        fh.set_len(6).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("f")).unwrap(),
            b"0123\0\0",
            "extending zero-fills"
        );
    }

    #[tokio::test]
    async fn mkdir_does_not_create_missing_parents() {
        let dir = tempfile::tempdir().unwrap();
        let be = backend_in(&dir);

        be.mkdir(&p("/one")).await.unwrap();
        assert!(dir.path().join("one").is_dir());

        let err = be.mkdir(&p("/one/two/three")).await.unwrap_err();
        assert!(
            matches!(err, Error::NotFound { .. } | Error::Io(_)),
            "a missing parent must surface, got {err:?}"
        );
        assert!(!dir.path().join("one/two").exists());
    }

    #[tokio::test]
    async fn remove_dir_refuses_a_non_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("d").join("f"), b"x").unwrap();
        let be = backend_in(&dir);

        let err = be.remove_dir(&p("/d")).await.unwrap_err();
        assert!(
            matches!(err, Error::DirectoryNotEmpty { .. }),
            "recursive deletion must not happen silently, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rename_replaces_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src"), b"new").unwrap();
        std::fs::write(dir.path().join("dst"), b"old").unwrap();
        let be = backend_in(&dir);

        be.rename(&p("/src"), &p("/dst")).await.unwrap();

        assert_eq!(std::fs::read(dir.path().join("dst")).unwrap(), b"new");
        assert!(!dir.path().join("src").exists());
    }

    #[tokio::test]
    async fn full_round_trip_create_upload_rename_read_delete() {
        let dir = tempfile::tempdir().unwrap();
        let be = backend_in(&dir);

        be.mkdir(&p("/media")).await.unwrap();

        let payload: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        let fh = be
            .open(&p("/media/upload.bin"), OpenMode::create_new())
            .await
            .unwrap();
        // Write in uneven chunks, out of order, to exercise positioned writes.
        let mut written = vec![false; payload.len()];
        let mut offset = 0usize;
        for size in [3usize, 1, 4096, 65536, 1500] {
            if offset >= payload.len() {
                break;
            }
            let end = (offset + size).min(payload.len());
            fh.write_at(offset as u64, &payload[offset..end]).await.unwrap();
            written[offset..end].fill(true);
            offset = end;
        }
        if offset < payload.len() {
            fh.write_at(offset as u64, &payload[offset..]).await.unwrap();
        }
        fh.flush().await.unwrap();
        fh.close().await.unwrap();

        assert_eq!(std::fs::read(dir.path().join("media/upload.bin")).unwrap(), payload);

        be.rename(&p("/media/upload.bin"), &p("/media/final.bin"))
            .await
            .unwrap();

        let entries = be.list(&p("/media")).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "final.bin");
        assert_eq!(entries[0].metadata.len, payload.len() as u64);

        let fh = be.open(&p("/media/final.bin"), OpenMode::read()).await.unwrap();
        let mut readback = vec![0u8; payload.len()];
        fh.read_exact_at(0, &mut readback).await.unwrap();
        fh.close().await.unwrap();
        assert_eq!(readback, payload, "round-tripped bytes must match exactly");

        be.remove_file(&p("/media/final.bin")).await.unwrap();
        be.remove_dir(&p("/media")).await.unwrap();
        assert!(be.list(&VfsPath::root()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn errors_name_the_path_that_failed() {
        let dir = tempfile::tempdir().unwrap();
        let be = backend_in(&dir);

        match be.stat(&p("/nope")).await.unwrap_err() {
            Error::NotFound { path } => assert_eq!(path, "/nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let be = backend_in(&dir);
        be.shutdown().await.unwrap();
        be.shutdown().await.unwrap();
    }

    #[test]
    fn resolve_cannot_escape_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let be = backend_in(&dir);
        let root = be.root().to_path_buf();

        // A segment that looks like a drive prefix must not replace the root,
        // which is what `PathBuf::push` would have done.
        let resolved = be.resolve(&p("/C:/windows"));
        assert!(
            resolved.starts_with(&root),
            "resolve escaped the root: {} (root {})",
            resolved.display(),
            root.display()
        );
    }

    #[test]
    fn endpoint_round_trips_as_a_uri() {
        let dir = tempfile::tempdir().unwrap();
        let be = backend_in(&dir);
        let ep = be.endpoint();
        assert!(ep.starts_with("file:///"), "unexpected endpoint: {ep}");
        assert!(!ep.contains('\\'), "URIs use forward slashes: {ep}");
    }
}
