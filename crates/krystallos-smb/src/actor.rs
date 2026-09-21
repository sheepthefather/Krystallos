//! The session thread: one libsmb2 context, one thread, commands over a channel.
//!
//! See the crate docs for why this is a dedicated thread and why the calls
//! inside it are blocking. The short version: libsmb2 has no locks, so a
//! context cannot be shared, and blocking calls let the thread park in `recv`
//! when idle instead of waking on a timer to check for work.

use crate::error;
use crate::path::{cstring, to_smb_path};
use krystallos_core::{Entry, EntryKind, Error, Metadata, Result, VfsPath};
use krystallos_sys_smb2::ffi;
use std::ffi::CStr;
use std::os::raw::c_int;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

/// Everything needed to open a session.
pub(crate) struct ConnectConfig {
    pub server: String,
    pub share: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub domain: Option<String>,
    /// Request SMB3 encryption. Must be set before connecting: it is negotiated
    /// during the handshake.
    pub seal: bool,
    pub timeout_secs: u32,
    /// The endpoint as the user wrote it, for error messages.
    pub label: String,
}

/// A libsmb2 context that destroys itself.
///
/// Not `Send`, and deliberately so: it holds a raw pointer to state with no
/// internal synchronisation. It is created on the session thread and dropped
/// there, and nothing else ever sees the pointer.
struct SmbContext {
    ptr: *mut ffi::smb2_context,
}

impl SmbContext {
    fn connect(config: &ConnectConfig) -> Result<Self> {
        // Winsock must be up before libsmb2 touches a socket, and libsmb2 does
        // not bring it up itself. A no-op everywhere else.
        if let Err(message) = krystallos_sys_smb2::ensure_network_ready() {
            return Err(Error::backend(message));
        }

        // SAFETY: every pointer passed below is either null or points to a
        // `CString` kept alive for the duration of the call, and the context is
        // used on this thread only.
        unsafe {
            let ptr = ffi::smb2_init_context();
            if ptr.is_null() {
                return Err(Error::backend(
                    "libsmb2 could not allocate a session context",
                ));
            }
            // From here the context exists and must be destroyed on every path
            // out, including the error ones below.
            let ctx = SmbContext { ptr };

            let user = config.user.as_deref().map(cstring).transpose()?;
            let password = config.password.as_deref().map(cstring).transpose()?;
            let domain = config.domain.as_deref().map(cstring).transpose()?;
            let server = cstring(&config.server)?;
            let share = cstring(&config.share)?;

            if let Some(c) = &user {
                ffi::smb2_set_user(ptr, c.as_ptr());
            }
            if let Some(c) = &password {
                ffi::smb2_set_password(ptr, c.as_ptr());
            }
            if let Some(c) = &domain {
                ffi::smb2_set_domain(ptr, c.as_ptr());
            }
            ffi::smb2_set_seal(ptr, c_int::from(config.seal));
            ffi::smb2_set_timeout(ptr, config.timeout_secs as c_int);

            let user_ptr = user.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
            let rc = ffi::smb2_connect_share(ptr, server.as_ptr(), share.as_ptr(), user_ptr);
            if rc != 0 {
                return Err(ctx.error(rc, &config.label));
            }

            Ok(ctx)
        }
    }

    fn ptr(&self) -> *mut ffi::smb2_context {
        self.ptr
    }

    fn error(&self, rc: i32, path: impl AsRef<str>) -> Error {
        error::from_context(self.ptr, rc, Some(path.as_ref()))
    }
}

impl Drop for SmbContext {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is a live context created by `smb2_init_context`
        // and destroyed exactly once, here, on the thread that owns it.
        unsafe {
            // Disconnecting first is courtesy to the server — it releases the
            // tree connect rather than waiting for the socket to drop.
            ffi::smb2_disconnect_share(self.ptr);
            ffi::smb2_destroy_context(self.ptr);
        }
    }
}

/// Work sent to the session thread.
enum Command {
    List {
        path: VfsPath,
        reply: oneshot::Sender<Result<Vec<Entry>>>,
    },
    Stat {
        path: VfsPath,
        reply: oneshot::Sender<Result<Metadata>>,
    },
    Mkdir {
        path: VfsPath,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveFile {
        path: VfsPath,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveDir {
        path: VfsPath,
        reply: oneshot::Sender<Result<()>>,
    },
    Rename {
        from: VfsPath,
        to: VfsPath,
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// A live SMB session.
pub(crate) struct Session {
    tx: Sender<Command>,
    /// Taken by [`Session::shutdown`] so the thread is joined exactly once.
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Session {
    pub(crate) async fn connect(config: ConnectConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let label = config.label.clone();

        let join = thread::Builder::new()
            .name(format!("krystallos-smb {label}"))
            .spawn(move || actor_thread(config, ready_tx, rx))
            .map_err(Error::Io)?;

        match ready_rx.await {
            Ok(Ok(())) => Ok(Session {
                tx,
                join: Mutex::new(Some(join)),
            }),
            // The thread reported a failure; it has already exited, so joining
            // it cannot block.
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            // The thread died before reporting: a panic in connect.
            Err(_) => {
                let _ = join.join();
                Err(Error::connection_lost(
                    "the SMB session thread exited before reporting a result",
                ))
            }
        }
    }

    /// Send a command and await its reply.
    ///
    /// A send failure means the session thread is gone — which is a session
    /// failure, not a per-operation one, so it is reported as such rather than
    /// as a broken pipe the caller would have to interpret.
    async fn call<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Command) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(make(reply)).map_err(|_| {
            Error::connection_lost("the SMB session thread has exited")
        })?;
        rx.await
            .map_err(|_| Error::connection_lost("the SMB session ended without replying"))?
    }

    pub(crate) async fn list(&self, path: &VfsPath) -> Result<Vec<Entry>> {
        self.call(|reply| Command::List {
            path: path.clone(),
            reply,
        })
        .await
    }

    pub(crate) async fn stat(&self, path: &VfsPath) -> Result<Metadata> {
        self.call(|reply| Command::Stat {
            path: path.clone(),
            reply,
        })
        .await
    }

    pub(crate) async fn mkdir(&self, path: &VfsPath) -> Result<()> {
        self.call(|reply| Command::Mkdir {
            path: path.clone(),
            reply,
        })
        .await
    }

    pub(crate) async fn remove_file(&self, path: &VfsPath) -> Result<()> {
        self.call(|reply| Command::RemoveFile {
            path: path.clone(),
            reply,
        })
        .await
    }

    pub(crate) async fn remove_dir(&self, path: &VfsPath) -> Result<()> {
        self.call(|reply| Command::RemoveDir {
            path: path.clone(),
            reply,
        })
        .await
    }

    pub(crate) async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.call(|reply| Command::Rename {
            from: from.clone(),
            to: to.clone(),
            reply,
        })
        .await
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        // A failed send means the thread already exited, which is the outcome
        // we wanted anyway.
        if self.tx.send(Command::Shutdown { reply }).is_ok() {
            let _ = rx.await;
        }

        let handle = self.join.lock().expect("session join lock poisoned").take();
        if let Some(handle) = handle {
            // Joining blocks, so it goes to the blocking pool rather than
            // stalling an async worker.
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
        Ok(())
    }
}

// Dropping a `Session` without calling `shutdown` drops the sender, which ends
// the actor's `recv` loop and destroys the context on its own thread. So the
// only thing `shutdown` adds is waiting for that to finish.

fn actor_thread(
    config: ConnectConfig,
    ready: oneshot::Sender<Result<()>>,
    rx: Receiver<Command>,
) {
    let ctx = match SmbContext::connect(&config) {
        Ok(ctx) => ctx,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    // `recv` blocks. That is the point: when there is no work the thread costs
    // nothing, and when a command arrives it wakes immediately rather than at
    // the next poll timeout.
    while let Ok(command) = rx.recv() {
        match command {
            Command::List { path, reply } => {
                let _ = reply.send(list_dir(&ctx, &path));
            }
            Command::Stat { path, reply } => {
                let _ = reply.send(stat_path(&ctx, &path));
            }
            Command::Mkdir { path, reply } => {
                let _ = reply.send(mkdir(&ctx, &path));
            }
            Command::RemoveFile { path, reply } => {
                let _ = reply.send(remove_file(&ctx, &path));
            }
            Command::RemoveDir { path, reply } => {
                let _ = reply.send(remove_dir(&ctx, &path));
            }
            Command::Rename { from, to, reply } => {
                let _ = reply.send(rename(&ctx, &from, &to));
            }
            Command::Shutdown { reply } => {
                let _ = reply.send(());
                break;
            }
        }
    }
    // `ctx` drops here, on the thread that owns it.
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

fn list_dir(ctx: &SmbContext, path: &VfsPath) -> Result<Vec<Entry>> {
    let smb_path = cstring(&to_smb_path(path))?;

    // SAFETY: `smb_path` outlives the call; the returned handle is either null
    // or owned by this thread until `smb2_closedir` below.
    let dir = unsafe { ffi::smb2_opendir(ctx.ptr(), smb_path.as_ptr()) };
    if dir.is_null() {
        // `smb2_opendir` reports failure as a null pointer, with no errno to
        // consult — so the status and message channels are all it has.
        return Err(ctx.error(0, path));
    }

    let mut entries = Vec::new();
    // SAFETY: `ctx` and `dir` are valid for the whole loop; the returned entry
    // is read and copied before the next call, which would invalidate it.
    unsafe {
        loop {
            let entry = ffi::smb2_readdir(ctx.ptr(), dir);
            if entry.is_null() {
                break;
            }
            let entry = &*entry;

            let name = if entry.name.is_null() {
                String::new()
            } else {
                CStr::from_ptr(entry.name).to_string_lossy().into_owned()
            };

            // SMB listings include the self and parent entries. The local
            // backend does not, and neither does any portable notion of a
            // directory listing, so they are dropped here — otherwise the same
            // directory would list differently depending on which backend
            // enumerated it.
            if name == "." || name == ".." {
                continue;
            }

            entries.push(Entry::new(name, metadata_from_stat(&entry.st)));
        }
        ffi::smb2_closedir(ctx.ptr(), dir);
    }

    // Sorted for determinism, matching the local backend. Backends are not
    // required to sort, but a stable order is what makes a differential test
    // between two backends meaningful.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn stat_path(ctx: &SmbContext, path: &VfsPath) -> Result<Metadata> {
    let smb_path = cstring(&to_smb_path(path))?;
    let mut st = ffi::smb2_stat_64::default();

    // SAFETY: `smb_path` and `st` both outlive the call.
    let rc = unsafe { ffi::smb2_stat(ctx.ptr(), smb_path.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(ctx.error(rc, path));
    }
    Ok(metadata_from_stat(&st))
}

fn mkdir(ctx: &SmbContext, path: &VfsPath) -> Result<()> {
    let smb_path = cstring(&to_smb_path(path))?;
    // SAFETY: `smb_path` outlives the call.
    let rc = unsafe { ffi::smb2_mkdir(ctx.ptr(), smb_path.as_ptr()) };
    if rc != 0 {
        return Err(ctx.error(rc, path));
    }
    Ok(())
}

fn remove_file(ctx: &SmbContext, path: &VfsPath) -> Result<()> {
    let smb_path = cstring(&to_smb_path(path))?;
    // SAFETY: `smb_path` outlives the call.
    let rc = unsafe { ffi::smb2_unlink(ctx.ptr(), smb_path.as_ptr()) };
    if rc != 0 {
        return Err(ctx.error(rc, path));
    }
    Ok(())
}

fn remove_dir(ctx: &SmbContext, path: &VfsPath) -> Result<()> {
    let smb_path = cstring(&to_smb_path(path))?;
    // SAFETY: `smb_path` outlives the call.
    let rc = unsafe { ffi::smb2_rmdir(ctx.ptr(), smb_path.as_ptr()) };
    if rc != 0 {
        return Err(ctx.error(rc, path));
    }
    Ok(())
}

fn rename(ctx: &SmbContext, from: &VfsPath, to: &VfsPath) -> Result<()> {
    let smb_from = cstring(&to_smb_path(from))?;
    let smb_to = cstring(&to_smb_path(to))?;
    // SAFETY: both C strings outlive the call.
    let rc = unsafe { ffi::smb2_rename(ctx.ptr(), smb_from.as_ptr(), smb_to.as_ptr()) };
    if rc != 0 {
        // Report the source: it is the path the caller can act on, and the
        // error text from libsmb2 usually names the actual problem anyway.
        return Err(ctx.error(rc, from));
    }
    Ok(())
}

/// Translate libsmb2's metadata into the portable model.
///
/// This is the boundary that keeps `smb2_attributes` and `smb2_reparse_tag`
/// from leaking upward — see ARCHITECTURE.md on what must not escape a backend.
fn metadata_from_stat(st: &ffi::smb2_stat_64) -> Metadata {
    Metadata {
        kind: match st.smb2_type {
            ffi::SMB2_TYPE_DIRECTORY => EntryKind::Directory,
            ffi::SMB2_TYPE_FILE => EntryKind::File,
            ffi::SMB2_TYPE_LINK => EntryKind::Symlink,
            // FIFOs, devices and sockets have no portable equivalent.
            _ => EntryKind::Other,
        },
        len: st.smb2_size,
        modified: timestamp(st.smb2_mtime, st.smb2_mtime_nsec),
        created: timestamp(st.smb2_btime, st.smb2_btime_nsec),
        accessed: timestamp(st.smb2_atime, st.smb2_atime_nsec),
        read_only: st.smb2_attributes & ffi::SMB2_FILE_ATTRIBUTE_READONLY != 0,
    }
}

/// Convert a libsmb2 timestamp, treating zero as "not reported".
///
/// SMB reports unset timestamps as zero, which would otherwise render as 1970
/// and look like a real — if implausible — answer.
fn timestamp(secs: u64, nsecs: u64) -> Option<SystemTime> {
    if secs == 0 {
        return None;
    }
    // `Duration::new` panics above 10^9 nanoseconds. A server sending a
    // nonsensical value must not take down the session thread over a cosmetic
    // field, so fall back to whole seconds.
    let nanos = u32::try_from(nsecs)
        .ok()
        .filter(|n| *n < 1_000_000_000);
    match nanos {
        Some(n) => UNIX_EPOCH.checked_add(Duration::new(secs, n)),
        None => UNIX_EPOCH.checked_add(Duration::from_secs(secs)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_treat_zero_as_absent() {
        assert!(timestamp(0, 0).is_none());
        assert!(timestamp(0, 123).is_none());
    }

    #[test]
    fn timestamps_round_trip_a_real_value() {
        let t = timestamp(1_700_000_000, 500).expect("should be Some");
        let d = t.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(d.as_secs(), 1_700_000_000);
        assert_eq!(d.subsec_nanos(), 500);
    }

    #[test]
    fn an_out_of_range_nanosecond_field_falls_back_rather_than_panicking() {
        // Duration::new would panic on this. A malformed server response must
        // not take the session down.
        let t = timestamp(1_700_000_000, 5_000_000_000).expect("should still be Some");
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_700_000_000
        );
    }

    #[test]
    fn kind_maps_every_smb2_type() {
        let mut st = ffi::smb2_stat_64::default();

        st.smb2_type = ffi::SMB2_TYPE_DIRECTORY;
        assert_eq!(metadata_from_stat(&st).kind, EntryKind::Directory);
        st.smb2_type = ffi::SMB2_TYPE_FILE;
        assert_eq!(metadata_from_stat(&st).kind, EntryKind::File);
        st.smb2_type = ffi::SMB2_TYPE_LINK;
        assert_eq!(metadata_from_stat(&st).kind, EntryKind::Symlink);
        // Anything without a portable equivalent must not be reported as a
        // file, or callers will try to open a named pipe.
        for exotic in [
            ffi::SMB2_TYPE_FIFO,
            ffi::SMB2_TYPE_CHARDEV,
            ffi::SMB2_TYPE_BLOCKDEV,
            ffi::SMB2_TYPE_SOCKET,
            9999,
        ] {
            st.smb2_type = exotic;
            assert_eq!(metadata_from_stat(&st).kind, EntryKind::Other);
        }
    }

    #[test]
    fn size_and_read_only_come_across() {
        let mut st = ffi::smb2_stat_64::default();
        st.smb2_type = ffi::SMB2_TYPE_FILE;
        st.smb2_size = 4_294_967_296;
        assert_eq!(metadata_from_stat(&st).len, 4_294_967_296);
        assert!(!metadata_from_stat(&st).read_only);

        st.smb2_attributes = ffi::SMB2_FILE_ATTRIBUTE_READONLY;
        assert!(metadata_from_stat(&st).read_only);

        // A directory attribute bit must not be mistaken for read-only.
        st.smb2_attributes = ffi::SMB2_FILE_ATTRIBUTE_DIRECTORY;
        assert!(!metadata_from_stat(&st).read_only);
    }
}
