//! End-to-end read-ahead tests against a live share.
//!
//! These measure the thing read-ahead actually claims to change — the number of
//! round-trips a sequential read costs — rather than wall-clock time. On a
//! loopback link the bytes move faster than the round-trips matter, so timing
//! shows almost nothing; the round-trip count shows the whole effect.
//!
//! Skipped unless `KRYSTALLOS_TEST_SMB_URI` is set, like the other live tests.

use krystallos_cache::ReadAhead;
use krystallos_core::{BackendRegistry, Credentials, FileHandle, OpenMode, VfsPath};
use krystallos_smb::SmbDriver;

fn live_uri() -> Option<String> {
    std::env::var("KRYSTALLOS_TEST_SMB_URI").ok()
}

fn credentials() -> Credentials {
    Credentials {
        username: std::env::var("KRYSTALLOS_TEST_SMB_USER").ok(),
        password: std::env::var("KRYSTALLOS_TEST_SMB_PASSWORD").ok(),
        domain: std::env::var("KRYSTALLOS_TEST_SMB_DOMAIN").ok(),
    }
}

/// Read the whole file in `chunk`-sized steps, returning the bytes.
async fn read_all(handle: &dyn FileHandle, chunk: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = handle.read_at(out.len() as u64, &mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

#[tokio::test]
async fn read_ahead_cuts_the_round_trips_a_sequential_read_costs() {
    let Some(uri) = live_uri() else {
        eprintln!("skipped: KRYSTALLOS_TEST_SMB_URI is not set");
        return;
    };

    let mut registry = BackendRegistry::new();
    registry.register(Box::new(SmbDriver::new()));
    let backend = registry
        .connect(&uri, &credentials())
        .await
        .expect("connect");

    let path = VfsPath::new("/big.bin").unwrap();

    // The access pattern a media player uses: reads far smaller than a window,
    // walking forward.
    const CHUNK: usize = 64 * 1024;

    let plain = backend
        .open(&path, OpenMode::read())
        .await
        .expect("open");
    let baseline = read_all(plain.as_ref(), CHUNK).await;
    plain.close().await.expect("close");

    let buffered = ReadAhead::with_window(
        backend
            .open(&path, OpenMode::read())
            .await
            .expect("open"),
        1024 * 1024,
    );
    let through_cache = read_all(&buffered, CHUNK).await;

    assert_eq!(
        through_cache, baseline,
        "read-ahead must not change the bytes"
    );

    let plain_reads = baseline.len() / CHUNK + 1; // + the end-of-file probe
    let cached_reads = buffered.inner_read_count() as usize;

    assert!(
        cached_reads * 4 < plain_reads,
        "read-ahead should cut round-trips sharply: {cached_reads} vs {plain_reads}"
    );

    // The window is 1 MiB and the file 50 MiB, so roughly one fetch per window.
    let expected = baseline.len() / (1024 * 1024) + 2;
    assert!(
        cached_reads <= expected,
        "expected about {expected} fetches for {} bytes, got {cached_reads}",
        baseline.len()
    );

    eprintln!(
        "krystallos: {} bytes read in {CHUNK}-byte steps: {plain_reads} round-trips \
         without read-ahead, {cached_reads} with",
        baseline.len()
    );

    buffered.close().await.expect("close");
    backend.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn read_ahead_preserves_bytes_from_unaligned_offsets() {
    let Some(uri) = live_uri() else {
        eprintln!("skipped: KRYSTALLOS_TEST_SMB_URI is not set");
        return;
    };

    let mut registry = BackendRegistry::new();
    registry.register(Box::new(SmbDriver::new()));
    let backend = registry
        .connect(&uri, &credentials())
        .await
        .expect("connect");

    let path = VfsPath::new("/big.bin").unwrap();
    let buffered = ReadAhead::with_window(
        backend
            .open(&path, OpenMode::read())
            .await
            .expect("open"),
        128 * 1024,
    );

    // Offsets chosen to straddle window boundaries, which is where an
    // off-by-one in the window arithmetic would show up.
    for offset in [0u64, 1, 65_535, 131_072, 131_073, 300_000, 1_000_001] {
        let mut buf = vec![0u8; 4096];
        buffered.read_exact_at(offset, &mut buf).await.unwrap();

        // Compare against a direct read of the same range.
        let direct = backend
            .open(&path, OpenMode::read())
            .await
            .expect("open");
        let mut expected = vec![0u8; 4096];
        direct.read_exact_at(offset, &mut expected).await.unwrap();
        direct.close().await.expect("close");

        assert_eq!(
            buf, expected,
            "read-ahead returned different bytes at offset {offset}"
        );
    }

    buffered.close().await.expect("close");
    backend.shutdown().await.expect("shutdown");
}
