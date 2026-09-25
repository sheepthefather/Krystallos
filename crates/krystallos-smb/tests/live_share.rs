//! Integration tests against a live SMB share.
//!
//! These are skipped unless the environment says where to find one, so
//! `cargo test` stays green on a machine with no server:
//!
//! ```text
//! KRYSTALLOS_TEST_SMB_URI=smb://127.0.0.1/krystallos-test
//! KRYSTALLOS_TEST_SMB_USER=b
//! KRYSTALLOS_TEST_SMB_PASSWORD=b
//! KRYSTALLOS_TEST_LOCAL_URI=file:///D:/krystallos-test-data   # enables the differential test
//! ```
//!
//! The differential test is the point of the local backend existing: the same
//! directory enumerated through two backends must produce the same answer. That
//! single comparison checks both that the abstraction leaks nothing and that
//! the SMB implementation is right, which is far stronger than SMB checking
//! itself.

use krystallos_core::{BackendRegistry, Credentials, Entry, OpenMode, VfsPath};
use krystallos_local::LocalDriver;
use krystallos_smb::SmbDriver;

struct Live {
    uri: String,
    credentials: Credentials,
}

/// Returns `None` when no share is configured, printing why so a skipped run is
/// not mistaken for a passing one.
fn live_share() -> Option<Live> {
    let uri = std::env::var("KRYSTALLOS_TEST_SMB_URI").ok()?;
    let credentials = Credentials {
        username: std::env::var("KRYSTALLOS_TEST_SMB_USER").ok(),
        password: std::env::var("KRYSTALLOS_TEST_SMB_PASSWORD").ok(),
        domain: std::env::var("KRYSTALLOS_TEST_SMB_DOMAIN").ok(),
    };
    eprintln!("using live share {uri}");
    Some(Live { uri, credentials })
}

/// Print a notice instead of failing when the share is not configured.
macro_rules! require_share {
    () => {
        match live_share() {
            Some(live) => live,
            None => {
                eprintln!("skipped: KRYSTALLOS_TEST_SMB_URI is not set");
                return;
            }
        }
    };
}

fn registry() -> BackendRegistry {
    let mut reg = BackendRegistry::new();
    reg.register(Box::new(LocalDriver::new()));
    reg.register(Box::new(SmbDriver::new()));
    reg
}

/// Serialises every test in this file.
///
/// They all share one fixture directory, and `cargo test` runs tests in
/// parallel by default. A test that creates or renames a directory while
/// another is enumerating the same directory makes the comparison fail for a
/// reason that has nothing to do with either backend. That is not hypothetical:
/// it is what this file did before the lock existed, intermittently.
///
/// An intermittently failing test is worse than no test. It teaches people to
/// re-run until it passes, which is how a genuine failure eventually gets
/// ignored. The whole file runs in hundredths of a second, so serialising costs
/// nothing worth measuring.
fn fixture_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
    &LOCK
}

fn p(s: &str) -> VfsPath {
    VfsPath::new(s).expect("test path should be valid")
}

#[tokio::test]
async fn connects_lists_and_stats() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    let backend = registry()
        .connect(&live.uri, &live.credentials)
        .await
        .expect("connect should succeed");

    let root = backend.list(&VfsPath::root()).await.expect("list root");
    assert!(
        !root.is_empty(),
        "the test share should not be empty; create some files in it"
    );

    // Every entry must carry metadata, because that is what makes the listing
    // a single round-trip rather than N+1, which is the whole reason `Entry`
    // bundles the two.
    for entry in &root {
        assert!(!entry.name.is_empty());
        assert!(
            entry.metadata.is_dir() || entry.metadata.is_file(),
            "entry {} has no usable metadata",
            entry.name
        );
    }

    // A file we know is there from the fixture, with a known size.
    let big = root.iter().find(|e| e.name == "big.bin");
    if let Some(big) = big {
        assert_eq!(big.metadata.len, 52_428_800, "fixture file size");
        let stat = backend.stat(&p("/big.bin")).await.expect("stat big.bin");
        assert_eq!(stat.len, big.metadata.len, "stat agrees with the listing");
        assert!(stat.is_file());
    }

    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn directory_operations_round_trip() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    let backend = registry()
        .connect(&live.uri, &live.credentials)
        .await
        .expect("connect");

    // A unique name so leftovers from an aborted run cannot collide.
    let name = format!("/krystallos-test-{}", std::process::id());
    let created = p(&name);
    let renamed = p(&format!("{name}-moved"));

    // Clean up anything a previous aborted run left behind.
    let _ = backend.remove_dir(&created).await;
    let _ = backend.remove_dir(&renamed).await;

    backend.mkdir(&created).await.expect("mkdir");
    let stat = backend.stat(&created).await.expect("stat the new dir");
    assert!(stat.is_dir(), "a freshly created directory should report as one");

    // Creating it again must be reported as a collision, not as a generic
    // failure. libsmb2 only puts the status name in the message for this path,
    // so it exercises the message-parsing channel of the error mapper.
    let err = match backend.mkdir(&created).await {
        Ok(()) => panic!("a second mkdir must not succeed"),
        Err(e) => e,
    };
    assert!(
        matches!(err, krystallos_core::Error::AlreadyExists { .. }),
        "expected AlreadyExists, got {err:?}"
    );

    backend.rename(&created, &renamed).await.expect("rename");

    // The old name is gone and the new one is present.
    let entries = backend.list(&VfsPath::root()).await.expect("list root");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let moved = renamed.file_name().expect("has a name");
    assert!(names.contains(&moved), "{moved} missing from {names:?}");
    assert!(
        !names.contains(&created.file_name().unwrap()),
        "the old name should be gone"
    );

    backend.remove_dir(&renamed).await.expect("cleanup");
    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn failures_are_classified_not_merely_reported() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    let backend = registry()
        .connect(&live.uri, &live.credentials)
        .await
        .expect("connect");

    use krystallos_core::Error;

    // Each of these exercised a different channel of the error mapper when it
    // was found against a live server; see `error.rs`.
    match backend.stat(&p("/definitely-not-here-9f3a")).await {
        Err(Error::NotFound { .. }) => {}
        other => panic!("stat on a missing file should be NotFound, got {other:?}"),
    }

    match backend.list(&p("/definitely-not-a-directory-9f3a")).await {
        Err(Error::NotFound { .. }) => {}
        other => panic!("listing a missing directory should be NotFound, got {other:?}"),
    }

    // The session must still be usable after per-operation failures: none of
    // the above is fatal.
    backend
        .list(&VfsPath::root())
        .await
        .expect("the session should survive per-operation errors");

    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_bad_password_is_reported_as_authentication_not_as_a_network_fault() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    // Only meaningful when the share actually requires credentials.
    if live.credentials.username.is_none() {
        eprintln!("skipped: no credentials configured, so there is nothing to get wrong");
        return;
    }

    let wrong = Credentials {
        username: live.credentials.username.clone(),
        password: Some("definitely-not-the-password".to_string()),
        domain: live.credentials.domain.clone(),
    };

    // Not `expect_err`: that would need `Box<dyn StorageBackend>: Debug`, which
    // a trait object is not.
    let err = match registry().connect(&live.uri, &wrong).await {
        Ok(_) => panic!("connecting with a bad password must fail"),
        Err(e) => e,
    };

    // The status channel is definitive here, but libsmb2's own description is a
    // later symptom (the socket being torn down). If that text came through
    // bare, the reader would go and debug their network instead of their
    // password.
    assert!(
        matches!(err, krystallos_core::Error::Auth { .. }),
        "expected Auth, got {err:?}"
    );
}

#[tokio::test]
async fn the_two_backends_agree_on_the_same_directory() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    let Some(local_uri) = std::env::var("KRYSTALLOS_TEST_LOCAL_URI").ok() else {
        eprintln!("skipped: KRYSTALLOS_TEST_LOCAL_URI is not set");
        return;
    };

    let reg = registry();
    let smb = reg.connect(&live.uri, &live.credentials).await.expect("connect smb");
    let local = reg
        .connect(&local_uri, &Credentials::anonymous())
        .await
        .expect("connect local");

    let from_smb = smb.list(&VfsPath::root()).await.expect("list via smb");
    let from_local = local.list(&VfsPath::root()).await.expect("list via local");

    compare(&from_smb, &from_local);

    smb.shutdown().await.expect("shutdown smb");
    local.shutdown().await.expect("shutdown local");
}

/// Compare two listings, reporting every disagreement rather than stopping at
/// the first. A full diff is far more useful than a single mismatched line.
fn compare(a: &[Entry], b: &[Entry]) {
    let names = |v: &[Entry]| v.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
    assert_eq!(
        names(a),
        names(b),
        "the two backends disagree about which entries exist\n  smb:   {:?}\n  local: {:?}",
        names(a),
        names(b)
    );

    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(
            x.metadata.kind, y.metadata.kind,
            "{} is a different kind through each backend",
            x.name
        );
        assert_eq!(
            x.metadata.len, y.metadata.len,
            "{} has a different size through each backend",
            x.name
        );
        assert_eq!(
            x.metadata.read_only, y.metadata.read_only,
            "{} disagrees on read-only",
            x.name
        );
        // Modification times are compared to the second: the two backends get
        // them from different sources with different sub-second precision.
        let secs = |t: Option<std::time::SystemTime>| {
            t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
        };
        assert_eq!(
            secs(x.metadata.modified),
            secs(y.metadata.modified),
            "{} has a different modification time through each backend",
            x.name
        );
    }
}

/// Copying is server-side, which is the whole reason it is worth having.
///
/// This cannot measure whether the bytes travelled through the client — that
/// needs a packet capture. What it does check is that the copy is byte-exact,
/// that it refuses to overwrite, and that a refusal leaves the destination as
/// it was, which together cover the paths this code can get wrong.
#[tokio::test]
async fn copying_a_file_is_byte_exact_and_refuses_to_overwrite() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    let backend = registry()
        .connect(&live.uri, &live.credentials)
        .await
        .expect("connect");

    let src = p(&format!("/krystallos-copy-src-{}.bin", std::process::id()));
    let dst = p(&format!("/krystallos-copy-dst-{}.bin", std::process::id()));
    let _ = backend.remove_file(&src).await;
    let _ = backend.remove_file(&dst).await;

    // Bigger than one copy chunk, so the loop in `copy_server_side` has to
    // advance more than once.
    let payload: Vec<u8> = (0..2 * 1024 * 1024 + 4096)
        .map(|i| ((i * 17 + (i >> 5)) % 251) as u8)
        .collect();

    let handle = backend
        .open(&src, OpenMode::write().with_truncate())
        .await
        .expect("open source for write");
    let mut offset = 0usize;
    while offset < payload.len() {
        let n = handle.write_at(offset as u64, &payload[offset..]).await.expect("write");
        assert!(n > 0);
        offset += n;
    }
    handle.flush().await.expect("flush");
    handle.close().await.expect("close");

    let copied = backend.copy(&src, &dst).await.expect("copy");
    assert_eq!(copied, payload.len() as u64, "the copy reported the wrong size");

    let read_back = read_all(backend.as_ref(), &dst).await;
    assert_eq!(read_back, payload, "the copied bytes differ from the source");

    // Refusing to clobber is deliberate: a paste onto an existing film must not
    // silently destroy it. libsmb2 reports this through the create path, which
    // is the message-parsing channel of the error mapper.
    let err = match backend.copy(&src, &dst).await {
        Ok(_) => panic!("a copy must not overwrite an existing file"),
        Err(e) => e,
    };
    assert!(
        matches!(err, krystallos_core::Error::AlreadyExists { .. }),
        "expected AlreadyExists, got {err:?}"
    );

    // And the refusal left the first copy untouched rather than truncating it.
    assert_eq!(
        read_all(backend.as_ref(), &dst).await,
        payload,
        "a refused copy damaged the file that was already there"
    );

    // Copying something that is not there must not leave an empty destination.
    let missing = backend.copy(&p("/krystallos-does-not-exist"), &p("/krystallos-never-created")).await;
    assert!(missing.is_err(), "copying a missing file should fail");
    assert!(
        backend.stat(&p("/krystallos-never-created")).await.is_err(),
        "a failed copy left a destination behind"
    );

    // And neither must a copy that fails *after* the destination exists — which
    // is the case that matters, because the file is already there by then. A
    // directory is the easiest way to reach it: the source opens, the
    // destination is created, and the copy itself cannot succeed.
    let source_dir = p(&format!("/krystallos-copy-dir-{}", std::process::id()));
    let doomed = p(&format!("/krystallos-copy-doomed-{}", std::process::id()));
    backend.mkdir(&source_dir).await.expect("mkdir");
    let _ = backend.remove_file(&doomed).await;

    let failed = backend.copy(&source_dir, &doomed).await;
    assert!(failed.is_err(), "copying a directory should not succeed");
    assert!(
        backend.stat(&doomed).await.is_err(),
        "a part-way copy left a file behind — it would look like a real film in a listing"
    );

    backend.remove_dir(&source_dir).await.expect("cleanup src dir");

    backend.remove_file(&src).await.expect("cleanup src");
    backend.remove_file(&dst).await.expect("cleanup dst");
    backend.shutdown().await.expect("shutdown");
}

/// Read a whole file through the backend, following short reads.
async fn read_all(backend: &dyn krystallos_core::StorageBackend, path: &VfsPath) -> Vec<u8> {
    let handle = backend.open(path, OpenMode::read()).await.expect("open for read");
    let size = handle.len() as usize;
    let mut out = vec![0u8; size];
    let mut offset = 0usize;
    while offset < size {
        let n = handle.read_at(offset as u64, &mut out[offset..]).await.expect("read");
        if n == 0 {
            break;
        }
        offset += n;
    }
    out.truncate(offset);
    handle.close().await.expect("close");
    out
}

/// The diagnostics screen's data: what the handshake settled on.
#[tokio::test]
async fn a_session_reports_what_it_negotiated() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    let backend = registry()
        .connect(&live.uri, &live.credentials)
        .await
        .expect("connect");

    // Reached the way the FFI reaches it: through the escape hatch, because a
    // dialect is SMB's idea and does not belong on the portable contract.
    let smb = backend
        .as_any()
        .downcast_ref::<krystallos_smb::SmbBackend>()
        .expect("this is the SMB backend");

    let info = smb.info();
    assert!(
        info.dialect >= 0x0202,
        "a connected session should report a real dialect, got {:#06x}",
        info.dialect
    );
    assert!(info.max_read_size > 0, "no read size was negotiated");
    assert!(info.max_write_size > 0, "no write size was negotiated");

    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_written_file_reads_back_byte_for_byte() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    let backend = registry()
        .connect(&live.uri, &live.credentials)
        .await
        .expect("connect");

    let name = format!("/krystallos-io-{}.bin", std::process::id());
    let path = p(&name);
    let _ = backend.remove_file(&path).await;

    // Deliberately not a round number of chunks, and not a repeating pattern:
    // a short final chunk and non-repeating bytes are what catch an off-by-one
    // in the transfer loop.
    let payload: Vec<u8> = (0..3 * 1024 * 1024 + 12345)
        .map(|i| ((i * 31 + (i >> 7)) % 251) as u8)
        .collect();

    let handle = backend
        .open(&path, OpenMode::write().with_truncate())
        .await
        .expect("open for write");
    let mut offset = 0usize;
    while offset < payload.len() {
        let n = handle
            .write_at(offset as u64, &payload[offset..])
            .await
            .expect("write");
        assert!(n > 0, "a write that makes no progress would loop forever");
        offset += n;
    }
    handle.flush().await.expect("flush");
    handle.close().await.expect("close");

    let meta = backend.stat(&path).await.expect("stat after write");
    assert_eq!(
        meta.len,
        payload.len() as u64,
        "the file on the server should be exactly as long as what was written"
    );

    let handle = backend
        .open(&path, OpenMode::read())
        .await
        .expect("open for read");
    let mut readback = vec![0u8; payload.len()];
    handle
        .read_exact_at(0, &mut readback)
        .await
        .expect("read back");
    handle.close().await.expect("close");

    assert_eq!(readback, payload, "round-tripped bytes must match exactly");

    backend.remove_file(&path).await.expect("cleanup");
    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn reading_the_same_file_through_both_backends_gives_the_same_bytes() {
    let _guard = fixture_lock().lock().await;
    let live = require_share!();
    let Some(local_uri) = std::env::var("KRYSTALLOS_TEST_LOCAL_URI").ok() else {
        eprintln!("skipped: KRYSTALLOS_TEST_LOCAL_URI is not set");
        return;
    };

    let reg = registry();
    let smb = reg.connect(&live.uri, &live.credentials).await.expect("connect smb");
    let local = reg
        .connect(&local_uri, &Credentials::anonymous())
        .await
        .expect("connect local");

    // A few mebibytes: enough to span several transfer chunks, so a bug in
    // chunk-boundary handling shows up. More would only make the test slower.
    let path = p("/big.bin");
    const WINDOW: u64 = 3 * 1024 * 1024;

    let smb_handle = smb.open(&path, OpenMode::read()).await.expect("open via smb");
    let local_handle = local
        .open(&path, OpenMode::read())
        .await
        .expect("open via local");

    let mut from_smb = vec![0u8; WINDOW as usize];
    let mut from_local = vec![0u8; WINDOW as usize];
    smb_handle
        .read_exact_at(0, &mut from_smb)
        .await
        .expect("read via smb");
    local_handle
        .read_exact_at(0, &mut from_local)
        .await
        .expect("read via local");

    assert_eq!(
        from_smb, from_local,
        "the same file read through two backends produced different bytes"
    );

    // And the same must hold from an offset that is not a chunk boundary.
    let offset = 1_000_000u64;
    let mut smb_tail = vec![0u8; 4096];
    let mut local_tail = vec![0u8; 4096];
    smb_handle.read_exact_at(offset, &mut smb_tail).await.expect("smb tail");
    local_handle
        .read_exact_at(offset, &mut local_tail)
        .await
        .expect("local tail");
    assert_eq!(
        smb_tail, local_tail,
        "reads from a non-aligned offset disagree between backends"
    );

    smb_handle.close().await.expect("close smb");
    local_handle.close().await.expect("close local");
    smb.shutdown().await.expect("shutdown smb");
    local.shutdown().await.expect("shutdown local");
}
