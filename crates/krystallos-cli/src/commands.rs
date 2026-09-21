use crate::format::{entry_line, human_bytes, metadata_block};
use krystallos_core::{
    BackendRegistry, Credentials, Error, OpenMode, Result, StorageBackend, VfsPath,
};
use std::io::{IsTerminal, Read, Write};
use std::path::Path;

/// Transfer chunk size.
///
/// One mebibyte keeps the round-trip count proportional to `size / 1 MiB`
/// rather than to whatever buffer the caller happens to pass, which over a
/// network is the difference between a handful of requests and thousands. It is
/// the same reasoning that will drive the media player's read-ahead.
const CHUNK: usize = 1024 * 1024;

/// Report the outcome of the work, preferring it over a shutdown failure — if
/// both failed, the work's error is the one that explains why.
async fn finish(backend: Box<dyn StorageBackend>, outcome: Result<()>) -> Result<()> {
    match (outcome, backend.shutdown().await) {
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn parse(path: &str) -> Result<VfsPath> {
    VfsPath::new(path)
}

pub async fn ls(
    reg: &BackendRegistry,
    creds: &Credentials,
    uri: &str,
    path: &str,
) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        let path = parse(path)?;
        let entries = backend.list(&path).await?;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for entry in &entries {
            writeln!(out, "{}", entry_line(entry))?;
        }
        out.flush()?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

pub async fn stat(
    reg: &BackendRegistry,
    creds: &Credentials,
    uri: &str,
    path: &str,
) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        let path = parse(path)?;
        let meta = backend.stat(&path).await?;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        write!(out, "{}", metadata_block(path.as_str(), &meta))?;
        out.flush()?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

pub async fn cat(
    reg: &BackendRegistry,
    creds: &Credentials,
    uri: &str,
    path: &str,
) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        let path = parse(path)?;
        let handle = open_for_read(backend.as_ref(), &path).await?;

        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        loop {
            let n = handle.read_at(offset, &mut buf).await?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            offset += n as u64;
        }
        out.flush()?;
        handle.close().await?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

pub async fn get(
    reg: &BackendRegistry,
    creds: &Credentials,
    uri: &str,
    path: &str,
    dest: &Path,
) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        let path = parse(path)?;
        let handle = open_for_read(backend.as_ref(), &path).await?;
        let total = handle.len();

        let mut file = std::fs::File::create(dest)?;
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        while offset < total {
            let n = handle.read_at(offset, &mut buf).await?;
            if n == 0 {
                // A backend reporting a length larger than the data it will
                // serve: stop rather than spin forever on a short read.
                break;
            }
            file.write_all(&buf[..n])?;
            offset += n as u64;
            progress(offset, total);
        }
        file.flush()?;
        progress_done(offset, total);
        handle.close().await?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

pub async fn put(
    reg: &BackendRegistry,
    creds: &Credentials,
    uri: &str,
    path: &str,
    src: &Path,
) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        let path = parse(path)?;
        let mut file = std::fs::File::open(src)?;
        let total = file.metadata()?.len();

        let handle = backend
            .open(&path, OpenMode::write().with_truncate())
            .await?;

        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        loop {
            // `read` on a file may return short; loop until it reports EOF.
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = file.read(&mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let mut written = 0usize;
            while written < filled {
                let n = handle.write_at(offset, &buf[written..filled]).await?;
                if n == 0 {
                    return Err(Error::backend(
                        "backend accepted 0 bytes of a write and made no progress",
                    ));
                }
                written += n;
                offset += n as u64;
                progress(offset, total);
            }
        }

        handle.flush().await?;
        progress_done(offset, total);
        handle.close().await?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

pub async fn mkdir(
    reg: &BackendRegistry,
    creds: &Credentials,
    uri: &str,
    path: &str,
) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        backend.mkdir(&parse(path)?).await?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

pub async fn rm(reg: &BackendRegistry, creds: &Credentials, uri: &str, path: &str) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        backend.remove_file(&parse(path)?).await?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

pub async fn rmdir(reg: &BackendRegistry, creds: &Credentials, uri: &str, path: &str) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        backend.remove_dir(&parse(path)?).await?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

pub async fn mv(
    reg: &BackendRegistry,
    creds: &Credentials,
    uri: &str,
    from: &str,
    to: &str,
) -> Result<()> {
    let backend = reg.connect(uri, creds).await?;
    let outcome = async {
        backend.rename(&parse(from)?, &parse(to)?).await?;
        Ok(())
    }
    .await;
    finish(backend, outcome).await
}

/// Open a file for reading, rejecting a directory up front.
///
/// Without this the failure would come from the backend as whatever its
/// "cannot open a directory" error happens to be, which is less clear than
/// saying so here.
async fn open_for_read(
    backend: &dyn StorageBackend,
    path: &VfsPath,
) -> Result<Box<dyn krystallos_core::FileHandle>> {
    let meta = backend.stat(path).await?;
    if meta.is_dir() {
        return Err(Error::IsADirectory {
            path: path.to_string(),
        });
    }
    backend.open(path, OpenMode::read()).await
}

/// Live progress, but only when a human is watching.
///
/// Writing a progress bar into a pipe would corrupt output that a script or a
/// differential test is comparing, so this is gated on stderr being a terminal.
fn progress(done: u64, total: u64) {
    if !std::io::stderr().is_terminal() || total == 0 {
        return;
    }
    let pct = (done as f64 / total as f64) * 100.0;
    eprint!(
        "\r  {} / {} ({pct:>5.1}%)   ",
        human_bytes(done),
        human_bytes(total)
    );
}

fn progress_done(done: u64, total: u64) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let note = if total != 0 && done != total {
        format!(" (expected {})", human_bytes(total))
    } else {
        String::new()
    };
    eprintln!("\r  {} transferred{note}   ", human_bytes(done));
}
