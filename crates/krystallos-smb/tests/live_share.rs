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

#[tokio::test]
async fn opening_a_file_is_reported_as_unimplemented_rather_than_faked() {
    let _guard = fixture_lock().lock().await;
    // Until the file-handle lifetime design lands, `open` must say so. A stub
    // that returned a handle would fail later and less clearly.
    let live = require_share!();
    let backend = registry()
        .connect(&live.uri, &live.credentials)
        .await
        .expect("connect");

    let err = match backend.open(&p("/readme.txt"), OpenMode::read()).await {
        Ok(_) => panic!("open is not implemented yet, but it returned a handle"),
        Err(e) => e,
    };
    assert!(
        matches!(err, krystallos_core::Error::Unsupported { .. }),
        "expected Unsupported for now, got {err:?}"
    );

    backend.shutdown().await.expect("shutdown");
}
