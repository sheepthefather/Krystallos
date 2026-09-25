//! The session thread: one libsmb2 context, one thread, commands over a channel.
//!
//! See the crate docs for why this is a dedicated thread and why the calls
//! inside it are blocking. The short version: libsmb2 has no locks, so a
//! context cannot be shared, and blocking calls let the thread park in `recv`
//! when idle instead of waking on a timer to check for work.
//!
//! # Open files
//!
//! A `smb2fh` is a raw pointer into the context's memory. It cannot cross a
//! thread boundary and must be released on the thread that owns it, so callers
//! never see one: they get an opaque `u64` id and every operation on a file
//! comes back through this thread. [`OpenFiles`] is the table that maps one to
//! the other, and it is what makes `Drop` on a remote file possible at all.

use crate::error;
use crate::file::RemoteFile;
use crate::path::{cstring, to_smb_path};
use krystallos_core::{Entry, EntryKind, Error, Metadata, OpenMode, Result, VfsPath};
use krystallos_sys_smb2::ffi;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_int;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
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

    /// Largest read the negotiated dialect allows. Reads must be split to this.
    fn max_read_size(&self) -> u32 {
        // SAFETY: read-only query on a live context.
        unsafe { ffi::smb2_get_max_read_size(self.ptr) }
    }

    fn max_write_size(&self) -> u32 {
        // SAFETY: read-only query on a live context.
        unsafe { ffi::smb2_get_max_write_size(self.ptr) }
    }

    /// The dialect negotiated during the handshake, as an `SMB2_VERSION_*`
    /// constant. Fixed once connected.
    fn dialect(&self) -> u16 {
        // SAFETY: read-only query on a live context.
        unsafe { ffi::smb2_get_dialect(self.ptr) }
    }
}

impl Drop for SmbContext {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is a live context created by `smb2_init_context`
        // and destroyed exactly once, here, on the thread that owns it. Every
        // handle opened from it has been closed by `OpenFiles::close_all`
        // before this runs.
        unsafe {
            // Disconnecting first is courtesy to the server — it releases the
            // tree connect rather than waiting for the socket to drop.
            ffi::smb2_disconnect_share(self.ptr);
            ffi::smb2_destroy_context(self.ptr);
        }
    }
}

/// The open-file table.
///
/// Maps the opaque ids callers hold onto libsmb2's raw handles, all of which
/// stay on this thread. Nothing else is allowed to store a `smb2fh`.
#[derive(Default)]
struct OpenFiles {
    handles: HashMap<u64, *mut ffi::smb2fh>,
    next_id: u64,
}

impl OpenFiles {
    fn insert(&mut self, handle: *mut ffi::smb2fh) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.handles.insert(id, handle);
        id
    }

    fn get(&self, id: u64) -> Option<*mut ffi::smb2fh> {
        self.handles.get(&id).copied()
    }

    fn take(&mut self, id: u64) -> Option<*mut ffi::smb2fh> {
        self.handles.remove(&id)
    }

    /// Close everything still open.
    ///
    /// Called before the context is destroyed. libsmb2 would reclaim them
    /// anyway when the session ends, but closing them properly tells the server
    /// to release its own state immediately rather than at session teardown —
    /// which matters on a server that limits concurrent open files.
    fn close_all(&mut self, ctx: &SmbContext) {
        for (_, handle) in self.handles.drain() {
            // SAFETY: each handle came from `smb2_open` on this thread and is
            // closed exactly once.
            unsafe {
                ffi::smb2_close(ctx.ptr(), handle);
            }
        }
    }
}

/// What an SMB session settled on, for showing to a user.
///
/// The fields are the protocol's own vocabulary, which is why this type lives
/// here and not in `krystallos-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmbInfo {
    /// The negotiated dialect, in SMB's numbering: `0x0311` is SMB 3.1.1,
    /// `0x0202` is SMB 2.0.2.
    pub dialect: u16,
    /// Largest single read the connection allows.
    pub max_read_size: u32,
    pub max_write_size: u32,
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
    Copy {
        from: VfsPath,
        to: VfsPath,
        /// Bytes copied.
        reply: oneshot::Sender<Result<u64>>,
    },
    Open {
        path: VfsPath,
        mode: OpenMode,
        /// `(handle id, size in bytes)`.
        reply: oneshot::Sender<Result<(u64, u64)>>,
    },
    Close {
        id: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    ReadAt {
        id: u64,
        offset: u64,
        len: u32,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    WriteAt {
        id: u64,
        offset: u64,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<usize>>,
    },
    SetLen {
        id: u64,
        len: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    Flush {
        id: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// A live SMB session.
///
/// Cheap to clone: every clone refers to the same session thread.
#[derive(Clone)]
pub(crate) struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    tx: Sender<Command>,
    /// Taken by [`Session::shutdown`] so the thread is joined exactly once.
    join: Mutex<Option<JoinHandle<()>>>,
    max_read_size: u32,
    max_write_size: u32,
    dialect: u16,
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
            Ok(Ok(limits)) => Ok(Session {
                inner: Arc::new(SessionInner {
                    tx,
                    join: Mutex::new(Some(join)),
                    max_read_size: limits.max_read,
                    max_write_size: limits.max_write,
                    dialect: limits.dialect,
                }),
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

    pub(crate) fn max_read_size(&self) -> u32 {
        self.inner.max_read_size
    }

    pub(crate) fn max_write_size(&self) -> u32 {
        self.inner.max_write_size
    }

    /// What this connection negotiated, for a diagnostics screen.
    pub(crate) fn info(&self) -> SmbInfo {
        SmbInfo {
            dialect: self.inner.dialect,
            max_read_size: self.inner.max_read_size,
            max_write_size: self.inner.max_write_size,
        }
    }

    /// Send a command and await its reply.
    ///
    /// A send failure means the session thread is gone — which is a session
    /// failure, not a per-operation one, so it is reported as such rather than
    /// as a broken pipe the caller would have to interpret.
    async fn call<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Command) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.inner.tx.send(make(reply)).map_err(|_| {
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

    /// Copy one file. Returns the bytes copied.
    pub(crate) async fn copy(&self, from: &VfsPath, to: &VfsPath) -> Result<u64> {
        self.call(|reply| Command::Copy {
            from: from.clone(),
            to: to.clone(),
            reply,
        })
        .await
    }

    /// Open a file, returning a handle whose lifetime is tied to this session.
    pub(crate) async fn open(&self, path: &VfsPath, mode: OpenMode) -> Result<RemoteFile> {
        let (id, len) = self
            .call(|reply| Command::Open {
                path: path.clone(),
                mode,
                reply,
            })
            .await?;
        Ok(RemoteFile::new(self.clone(), id, len, path.clone()))
    }

    pub(crate) async fn close(&self, id: u64) -> Result<()> {
        self.call(|reply| Command::Close { id, reply }).await
    }

    /// Queue a close without waiting for it.
    ///
    /// For `Drop`, which cannot await and may run on any thread — including a
    /// JVM finalizer thread with no async runtime in sight. Sending on the
    /// command channel is synchronous and never blocks, so no runtime is
    /// needed; the reply is simply dropped, and the session thread ignores a
    /// reply nobody is waiting for.
    pub(crate) fn close_detached(&self, id: u64) {
        let (reply, _) = oneshot::channel();
        // A failed send means the session thread is gone, and `close_all`
        // has already released every handle it held.
        let _ = self.inner.tx.send(Command::Close { id, reply });
    }

    pub(crate) async fn read_at(&self, id: u64, offset: u64, len: u32) -> Result<Vec<u8>> {
        self.call(|reply| Command::ReadAt {
            id,
            offset,
            len,
            reply,
        })
        .await
    }

    pub(crate) async fn write_at(&self, id: u64, offset: u64, data: Vec<u8>) -> Result<usize> {
        self.call(|reply| Command::WriteAt {
            id,
            offset,
            data,
            reply,
        })
        .await
    }

    pub(crate) async fn set_len(&self, id: u64, len: u64) -> Result<()> {
        self.call(|reply| Command::SetLen { id, len, reply }).await
    }

    pub(crate) async fn flush(&self, id: u64) -> Result<()> {
        self.call(|reply| Command::Flush { id, reply }).await
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        // A failed send means the thread already exited, which is the outcome
        // we wanted anyway.
        if self.inner.tx.send(Command::Shutdown { reply }).is_ok() {
            let _ = rx.await;
        }

        let handle = self
            .inner
            .join
            .lock()
            .expect("session join lock poisoned")
            .take();
        if let Some(handle) = handle {
            // Joining blocks, so it goes to the blocking pool rather than
            // stalling an async worker.
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
        Ok(())
    }
}

// Dropping the last `Session` clone drops the sender, which ends the actor's
// `recv` loop and destroys the context on its own thread. So the only thing
// `shutdown` adds is waiting for that to finish.

/// What the connection settled on, reported back once it is up.
///
/// Read once here rather than queried on demand: none of it changes after the
/// handshake, and a diagnostic screen should not cost a round-trip.
struct Limits {
    max_read: u32,
    max_write: u32,
    dialect: u16,
}

fn actor_thread(
    config: ConnectConfig,
    ready: oneshot::Sender<Result<Limits>>,
    rx: Receiver<Command>,
) {
    let ctx = match SmbContext::connect(&config) {
        Ok(ctx) => ctx,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let limits = Limits {
        max_read: ctx.max_read_size(),
        max_write: ctx.max_write_size(),
        dialect: ctx.dialect(),
    };
    let _ = ready.send(Ok(limits));

    let mut files = OpenFiles::default();

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
            Command::Copy { from, to, reply } => {
                let _ = reply.send(copy_file(&ctx, &from, &to));
            }
            Command::Open { path, mode, reply } => {
                let _ = reply.send(open(&ctx, &mut files, &path, mode));
            }
            Command::Close { id, reply } => {
                let _ = reply.send(close_file(&ctx, &mut files, id));
            }
            Command::ReadAt {
                id,
                offset,
                len,
                reply,
            } => {
                let _ = reply.send(read_at(&ctx, &files, id, offset, len));
            }
            Command::WriteAt {
                id,
                offset,
                data,
                reply,
            } => {
                let _ = reply.send(write_at(&ctx, &files, id, offset, &data));
            }
            Command::SetLen { id, len, reply } => {
                let _ = reply.send(set_len(&ctx, &files, id, len));
            }
            Command::Flush { id, reply } => {
                let _ = reply.send(flush(&ctx, &files, id));
            }
            Command::Shutdown { reply } => {
                let _ = reply.send(());
                break;
            }
        }
    }

    // Release every handle before the context goes: closing tells the server to
    // drop its own state now, rather than whenever the session finally tears
    // down.
    files.close_all(&ctx);
    // `ctx` drops here, on the thread that owns it.
}

// ---------------------------------------------------------------------------
// Path operations
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

    // Sorted for determinism, matching the local backend.
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

/// A file handle closed when it goes out of scope.
///
/// A copy holds two handles and has several ways to fail between opening them,
/// so closing them by hand at each exit would eventually miss one — and a
/// leaked handle on the server is not something the caller can see or fix.
struct FhGuard<'a> {
    ctx: &'a SmbContext,
    fh: *mut ffi::smb2fh,
}

impl Drop for FhGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: the handle came from `smb2_open` on this thread, and the
        // guard owns it so this is its only close.
        unsafe { ffi::smb2_close(self.ctx.ptr(), self.fh) };
    }
}

/// Initial per-request copy size.
///
/// The server gets to lower this; see [`copy_server_side`]. 1 MiB is the
/// transfer size the rest of the kernel already uses, so it is the obvious
/// first guess.
const COPY_CHUNK_BYTES: u32 = 1024 * 1024;

/// Copy a single file.
///
/// **Server-side, so the bytes never pass through this process.** SMB has a
/// filesystem control for exactly this: the source's *resume key* identifies it,
/// and a COPYCHUNK request tells the server to move ranges between its own
/// handles. A client-side loop would put a multi-gigabyte film across the
/// network twice — once down, once back up — which on Wi-Fi is the difference
/// between seconds and minutes.
///
/// Not every server implements it, so this falls back to reading and writing
/// through this process. The fallback is deliberately invisible above this
/// layer: the caller asked for a copy, not for a particular way of doing one.
///
/// Directories are not copied. Recursion is the caller's job, for the same
/// reason [`remove_dir`] refuses a non-empty directory — see the core crate's
/// documentation on keeping error handling visible at the call site.
fn copy_file(ctx: &SmbContext, from: &VfsPath, to: &VfsPath) -> Result<u64> {
    let smb_from = cstring(&to_smb_path(from))?;
    let smb_to = cstring(&to_smb_path(to))?;

    // SAFETY: both C strings outlive the calls; the handles are closed by the
    // guards on every path out of this function.
    unsafe {
        let src = ffi::smb2_open(ctx.ptr(), smb_from.as_ptr(), ffi::open_flags::O_RDONLY);
        if src.is_null() {
            return Err(ctx.error(0, from));
        }
        let src = FhGuard { ctx, fh: src };

        let mut st = ffi::smb2_stat_64::default();
        let size = if ffi::smb2_fstat(ctx.ptr(), src.fh, &mut st) == 0 {
            st.smb2_size
        } else {
            0
        };

        // `O_EXCL` rather than overwriting: a copy that silently replaced an
        // existing film would be the worst kind of surprise, and the caller can
        // see this error and decide what to do about it.
        let flags = ffi::open_flags::O_WRONLY | ffi::open_flags::O_CREAT | ffi::open_flags::O_EXCL;
        let dst = ffi::smb2_open(ctx.ptr(), smb_to.as_ptr(), flags);
        if dst.is_null() {
            return Err(ctx.error(0, to));
        }
        let dst = FhGuard { ctx, fh: dst };

        if size == 0 {
            // An empty source still means "create the destination", which the
            // open above has already done.
            return Ok(0);
        }

        let outcome = match copy_server_side(ctx, src.fh, dst.fh, size) {
            Ok(copied) => Ok(copied),
            Err(CopyFailed::Unsupported) => {
                // Worth saying out loud, because the difference between the two
                // paths is orders of magnitude on a large film and nothing else
                // in the result would reveal which one ran.
                if std::env::var_os("KRYSTALLOS_DEBUG").is_some() {
                    eprintln!("krystallos: {to}: server-side copy unsupported, moving the bytes");
                }
                copy_through_here(ctx, src.fh, dst.fh, size)
            }
            Err(CopyFailed::Error(e)) => Err(e),
        };

        // A copy that fails part-way leaves a short or empty file behind, and
        // that is worse than leaving nothing: the name is there, the size looks
        // plausible in a listing, and the only way to find out is to play it and
        // watch it stop. So the partial destination is removed before the error
        // is returned. It is ours to remove — it was created here with O_EXCL.
        if outcome.is_err() {
            // The handle must go before the unlink: SMB will not remove a file
            // it still holds open.
            drop(dst);
            ffi::smb2_unlink(ctx.ptr(), smb_to.as_ptr());
        }
        outcome
    }
}

/// Why a server-side copy did not finish.
enum CopyFailed {
    /// The server does not implement COPYCHUNK, so the bytes have to move.
    Unsupported,
    /// Something else went wrong, and moving the bytes would not help.
    Error(Error),
}

/// Ask the server to copy [size] bytes from `src` to `dst`.
///
/// # The limit negotiation
///
/// A server has its own maximum chunk size, and the client is expected to find
/// it by asking for too much: the refusal comes back as an error *with the
/// limits filled into the same reply struct* that a success uses. That is why
/// the reply is inspected on the error path here rather than discarded — and
/// why the adopted limit is only ever accepted when it is **smaller** than what
/// was just tried, which is what stops a server that answers every request with
/// an error from becoming an infinite loop.
///
/// # Safety
///
/// `src` and `dst` must be handles open on `ctx`, on this thread.
unsafe fn copy_server_side(
    ctx: &SmbContext,
    src: *mut ffi::smb2fh,
    dst: *mut ffi::smb2fh,
    size: u64,
) -> std::result::Result<u64, CopyFailed> {
    let mut key = ffi::smb2_srv_copychunk_resume_key {
        resume_key: [0; ffi::SMB2_SRV_COPYCHUNK_RESUME_KEY_SIZE],
    };
    // SAFETY: the caller guarantees both handles are open on this context.
    let rc = unsafe { ffi::smb2_request_resume_key(ctx.ptr(), src, &mut key) };
    if rc != 0 {
        return Err(classify_copy_failure(ctx, rc));
    }

    let mut chunk_bytes = COPY_CHUNK_BYTES;
    let mut offset = 0u64;
    while offset < size {
        let length = chunk_bytes.min((size - offset).min(u32::MAX as u64) as u32);
        let chunk = ffi::smb2_srv_copychunk {
            source_offset: offset,
            target_offset: offset,
            length,
            reserved: 0,
        };
        let mut reply = ffi::smb2_srv_copychunk_reply::default();
        // SAFETY: as above; `key` and `chunk` outlive the call, and `reply` is
        // written by libsmb2 and stayed owned here.
        let rc = unsafe {
            ffi::smb2_copychunk(
                ctx.ptr(),
                ffi::SMB2_FSCTL_SRV_COPYCHUNK_WRITE,
                &key,
                dst,
                &chunk,
                1,
                &mut reply,
            )
        };

        if rc == 0 {
            let written = u64::from(reply.total_bytes_written);
            if written == 0 {
                // A success that moved nothing would loop forever. The server
                // is misbehaving; report it rather than spinning.
                return Err(CopyFailed::Error(Error::backend(
                    "the server reported a copy of zero bytes",
                )));
            }
            offset += written;
            continue;
        }

        // An error carrying a usable limit means "not that much at a time".
        let limit = reply.chunk_bytes_written;
        if limit > 0 && limit < chunk_bytes {
            chunk_bytes = limit;
            continue;
        }
        return Err(classify_copy_failure(ctx, rc));
    }
    Ok(offset)
}

/// Whether a status means "this server does not do server-side copies".
///
/// Split out from the classification below so it can be tested without a
/// server: the three codes are the ones Windows and Samba use for a filesystem
/// control they do not implement, and treating one of them as a hard failure
/// would break copying against those servers entirely.
fn is_copy_unsupported(status: u32) -> bool {
    matches!(
        status,
        ffi::SMB2_STATUS_NOT_SUPPORTED
            | ffi::SMB2_STATUS_INVALID_DEVICE_REQUEST
            | ffi::SMB2_STATUS_CTL_FILE_NOT_SUPPORTED
    )
}

/// Decide whether a failed copy is worth retrying without the server's help.
fn classify_copy_failure(ctx: &SmbContext, rc: i32) -> CopyFailed {
    // SAFETY: a read-only query on a live context.
    let status = unsafe { ffi::smb2_get_nterror(ctx.ptr()) } as u32;
    if is_copy_unsupported(status) {
        return CopyFailed::Unsupported;
    }
    CopyFailed::Error(ctx.error(rc, "copy"))
}

/// The fallback: read the source and write the destination from this process.
///
/// Slower by orders of magnitude on a large file, since every byte crosses the
/// network twice. It exists so that a server without COPYCHUNK still gets a
/// working copy rather than an error.
///
/// # Safety
///
/// `src` and `dst` must be handles open on `ctx`, on this thread, and `dst` must
/// be empty — this writes from offset zero and does not truncate.
unsafe fn copy_through_here(
    ctx: &SmbContext,
    src: *mut ffi::smb2fh,
    dst: *mut ffi::smb2fh,
    size: u64,
) -> Result<u64> {
    let mut buf = vec![0u8; COPY_CHUNK_BYTES as usize];
    let mut offset = 0u64;
    while offset < size {
        let want = (size - offset).min(buf.len() as u64) as u32;
        // SAFETY: the caller guarantees both handles are open on this context,
        // and `buf` is uniquely borrowed here.
        let read = unsafe { ffi::smb2_pread(ctx.ptr(), src, buf.as_mut_ptr(), want, offset) };
        if read < 0 {
            return Err(ctx.error(read, "copy (read)"));
        }
        if read == 0 {
            // The source shrank under us. Whatever was copied is all there is.
            break;
        }
        let mut written = 0i32;
        while written < read {
            // SAFETY: as above; the offset is inside `buf`.
            let n = unsafe {
                ffi::smb2_pwrite(
                    ctx.ptr(),
                    dst,
                    buf.as_ptr().add(written as usize),
                    (read - written) as u32,
                    offset + written as u64,
                )
            };
            if n <= 0 {
                return Err(ctx.error(n, "copy (write)"));
            }
            written += n;
        }
        offset += read as u64;
    }
    Ok(offset)
}

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

/// Translate [`OpenMode`] into the POSIX flags libsmb2 expects.
///
/// The values come from the target platform (`ffi::open_flags`), because
/// libsmb2 interprets whatever number it receives using the constants it was
/// compiled against.
fn open_flags(mode: OpenMode) -> c_int {
    use ffi::open_flags as f;

    let mut flags = match (mode.is_read(), mode.is_write()) {
        (true, true) => f::O_RDWR,
        (false, true) => f::O_WRONLY,
        // Neither was requested, which the mode constructors do not produce;
        // opening read-only is the harmless reading of an impossible input.
        (true, false) | (false, false) => f::O_RDONLY,
    };

    if mode.creates() {
        flags |= f::O_CREAT;
    }
    if mode.must_not_exist() {
        flags |= f::O_EXCL;
    }
    if mode.truncates() {
        flags |= f::O_TRUNC;
    }
    flags
}

fn open(
    ctx: &SmbContext,
    files: &mut OpenFiles,
    path: &VfsPath,
    mode: OpenMode,
) -> Result<(u64, u64)> {
    let smb_path = cstring(&to_smb_path(path))?;
    // SAFETY: `smb_path` outlives the call; the handle is either null or owned
    // by this thread and stored in `files` below.
    let handle = unsafe { ffi::smb2_open(ctx.ptr(), smb_path.as_ptr(), open_flags(mode)) };
    if handle.is_null() {
        return Err(ctx.error(0, path));
    }

    // The size is a separate query: the create response carries it, but the
    // synchronous API does not surface it.
    //
    // A failure here is *not* fatal to the open. A file opened write-only may
    // not be queryable, and the size is only advisory — `FileHandle::len` is
    // documented as the size seen at open, and no read path depends on it for
    // correctness (reads run until the backend says end-of-file).
    let mut st = ffi::smb2_stat_64::default();
    // SAFETY: `handle` was just opened on this thread and `st` outlives the call.
    let len = match unsafe { ffi::smb2_fstat(ctx.ptr(), handle, &mut st) } {
        0 => st.smb2_size,
        _ => 0,
    };

    Ok((files.insert(handle), len))
}

fn close_file(ctx: &SmbContext, files: &mut OpenFiles, id: u64) -> Result<()> {
    let Some(handle) = files.take(id) else {
        // Already closed, or the id was never valid. Idempotent by design: the
        // trait requires `close` to be safe to call twice, and a dropped file
        // may already have sent this.
        return Ok(());
    };
    // SAFETY: the handle came from `smb2_open` on this thread and is closed
    // exactly once — `take` removed it from the table.
    let rc = unsafe { ffi::smb2_close(ctx.ptr(), handle) };
    if rc != 0 {
        return Err(ctx.error(rc, format!("file handle {id}")));
    }
    Ok(())
}

/// Look up a handle, mapping a stale id to a clear error rather than a crash.
fn handle_of(files: &OpenFiles, id: u64, path: &str) -> Result<*mut ffi::smb2fh> {
    files.get(id).ok_or_else(|| Error::Backend {
        message: format!("operation on {path} used a file handle that is not open"),
    })
}

fn read_at(
    ctx: &SmbContext,
    files: &OpenFiles,
    id: u64,
    offset: u64,
    len: u32,
) -> Result<Vec<u8>> {
    let handle = handle_of(files, id, "read")?;
    let mut buf = vec![0u8; len as usize];

    // SAFETY: `handle` is open on this thread, and `buf` is exclusively owned
    // here with a length matching the `count` passed.
    let rc = unsafe {
        ffi::smb2_pread(
            ctx.ptr(),
            handle,
            buf.as_mut_ptr(),
            len,
            offset,
        )
    };
    if rc < 0 {
        return Err(ctx.error(rc, "read"));
    }

    buf.truncate(rc as usize);
    Ok(buf)
}

fn write_at(
    ctx: &SmbContext,
    files: &OpenFiles,
    id: u64,
    offset: u64,
    data: &[u8],
) -> Result<usize> {
    let handle = handle_of(files, id, "write")?;

    // SAFETY: `handle` is open on this thread, and `data` is a shared slice
    // valid for the duration of the call. libsmb2 treats it as read-only.
    let rc = unsafe {
        ffi::smb2_pwrite(
            ctx.ptr(),
            handle,
            data.as_ptr(),
            data.len() as u32,
            offset,
        )
    };
    if rc < 0 {
        return Err(ctx.error(rc, "write"));
    }
    Ok(rc as usize)
}

fn set_len(ctx: &SmbContext, files: &OpenFiles, id: u64, len: u64) -> Result<()> {
    let handle = handle_of(files, id, "truncate")?;
    // SAFETY: `handle` is open on this thread.
    let rc = unsafe { ffi::smb2_ftruncate(ctx.ptr(), handle, len) };
    if rc != 0 {
        return Err(ctx.error(rc, "truncate"));
    }
    Ok(())
}

fn flush(ctx: &SmbContext, files: &OpenFiles, id: u64) -> Result<()> {
    let handle = handle_of(files, id, "flush")?;
    // SAFETY: `handle` is open on this thread.
    let rc = unsafe { ffi::smb2_fsync(ctx.ptr(), handle) };
    if rc != 0 {
        return Err(ctx.error(rc, "flush"));
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
    let nanos = u32::try_from(nsecs).ok().filter(|n| *n < 1_000_000_000);
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
        assert_eq!(t.duration_since(UNIX_EPOCH).unwrap().as_secs(), 1_700_000_000);
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

    #[test]
    fn open_flags_select_the_right_access_mode() {
        use ffi::open_flags as f;
        assert_eq!(open_flags(OpenMode::read()), f::O_RDONLY);

        let write = open_flags(OpenMode::write());
        assert_eq!(write & 0b11, f::O_WRONLY, "write mode must not request read");
        assert_ne!(write & f::O_CREAT, 0);

        let rw = open_flags(OpenMode::read_write());
        assert_eq!(rw & 0b11, f::O_RDWR);
    }

    #[test]
    fn exclusive_creation_sets_the_exclusive_flag() {
        use ffi::open_flags as f;
        let flags = open_flags(OpenMode::create_new());
        assert_ne!(flags & f::O_CREAT, 0);
        assert_ne!(flags & f::O_EXCL, 0);
        assert_eq!(flags & f::O_TRUNC, 0, "create_new must not truncate");
    }

    #[test]
    fn truncation_is_only_requested_when_asked_for() {
        use ffi::open_flags as f;
        assert_eq!(open_flags(OpenMode::read_write()) & f::O_TRUNC, 0);
        assert_ne!(
            open_flags(OpenMode::read_write().with_truncate()) & f::O_TRUNC,
            0
        );
    }

    #[test]
    fn the_handle_table_hands_out_distinct_ids() {
        let mut files = OpenFiles::default();
        let a = files.insert(std::ptr::null_mut());
        let b = files.insert(std::ptr::null_mut());
        assert_ne!(a, b);
        assert!(files.get(a).is_some());
        assert!(files.take(a).is_some());
        assert!(files.get(a).is_none(), "a taken handle must be gone");
        assert!(files.get(b).is_some(), "taking one must not disturb another");
    }

    /// A session whose "thread" is the returned receiver, so a test can see
    /// exactly which commands were sent without a server.
    fn session_with_inbox() -> (Session, Receiver<Command>) {
        let (tx, rx) = mpsc::channel();
        let session = Session {
            inner: Arc::new(SessionInner {
                tx,
                join: Mutex::new(None),
                max_read_size: 65536,
                max_write_size: 65536,
                dialect: 0x0311,
            }),
        };
        (session, rx)
    }

    #[test]
    fn dropping_a_file_outside_any_runtime_still_closes_it() {
        // A plain `#[test]`, not `#[tokio::test]`, on purpose: through the FFI
        // a file is dropped on whichever thread released the Kotlin object,
        // which has no runtime. A close that needed one was silently skipped
        // there, leaving the server-side handle open until disconnect.
        let (session, inbox) = session_with_inbox();
        let file = RemoteFile::new(session, 7, 0, VfsPath::new("/a.mkv").unwrap());
        drop(file);

        match inbox.try_recv() {
            Ok(Command::Close { id, .. }) => assert_eq!(id, 7),
            Ok(_) => panic!("dropping a file sent something other than a close"),
            Err(e) => panic!("dropping a file sent no close: {e}"),
        }
        assert!(inbox.try_recv().is_err(), "exactly one close per file");
    }

    #[tokio::test]
    async fn an_explicit_close_is_not_repeated_by_drop() {
        let (session, inbox) = session_with_inbox();
        // Answer the explicit close the way the session thread would.
        let responder = thread::spawn(move || {
            let mut closes = 0;
            while let Ok(command) = inbox.recv() {
                if let Command::Close { reply, .. } = command {
                    closes += 1;
                    let _ = reply.send(Ok(()));
                }
            }
            closes
        });

        let file = RemoteFile::new(session, 3, 0, VfsPath::new("/a.mkv").unwrap());
        krystallos_core::FileHandle::close(&file).await.unwrap();
        drop(file);

        assert_eq!(responder.join().unwrap(), 1, "close then drop must close once");
    }

    #[test]
    fn a_server_that_cannot_copy_server_side_is_recognised() {
        // What Windows and Samba answer for a filesystem control they do not
        // implement. Missing one means copying against such a server fails
        // outright instead of falling back to moving the bytes.
        for status in [
            ffi::SMB2_STATUS_NOT_SUPPORTED,
            ffi::SMB2_STATUS_INVALID_DEVICE_REQUEST,
            ffi::SMB2_STATUS_CTL_FILE_NOT_SUPPORTED,
        ] {
            assert!(is_copy_unsupported(status), "{status:#x} should fall back");
        }
    }

    #[test]
    fn other_failures_do_not_fall_back() {
        // Falling back on these would push a multi-gigabyte film through this
        // process to fix an error that moving bytes cannot fix — a permission
        // failure, or a name that is already taken.
        for status in [
            ffi::SMB2_STATUS_INVALID_PARAMETER,
            0xC000_0034, // STATUS_OBJECT_NAME_NOT_FOUND
            0xDEAD_BEEF,
        ] {
            assert!(!is_copy_unsupported(status), "{status:#x} should not fall back");
        }
    }

    #[test]
    fn a_stale_handle_is_an_error_not_a_crash() {
        let files = OpenFiles::default();
        match handle_of(&files, 42, "read") {
            Err(Error::Backend { message }) => assert!(message.contains("not open")),
            other => panic!("expected a clear error, got {other:?}"),
        }
    }
}
