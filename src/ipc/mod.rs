//! Per-user IPC and the single-instance guarantee (§24, §25, §28).
//!
//! One daemon per user session owns a Unix domain socket in
//! `$XDG_RUNTIME_DIR`. The CLI connects, sends one request, reads one response
//! and disconnects. Both halves ship in the same binary, which is why
//! `clipboard-manager toggle` from a desktop shortcut reaches the daemon that
//! is already monitoring rather than starting a second monitor (§23).
//!
//! # Why a socket and not D-Bus
//!
//! D-Bus would be the conventional choice and was rejected deliberately. A
//! session bus name is visible to every process on the session bus, including
//! sandboxed applications with bus access, and the interface would have to
//! expose history content to be useful. A socket in `$XDG_RUNTIME_DIR` is
//! reachable only by this user — the kernel enforces it through directory
//! permissions, with nothing for us to get wrong — and §28 requires exactly
//! that. It also means no dependency on a running bus, so the daemon works in
//! a bare session.
//!
//! # Single instance
//!
//! An advisory `flock(2)` on a lock file beside the socket. Chosen over a
//! pidfile because the kernel releases it when the holder dies, however it
//! dies: no stale lock after a crash, no PID-reuse race, no cleanup code to be
//! wrong. A second launch fails to take the lock, sends [`Request::Show`] to
//! the incumbent and exits — which is what makes clicking the desktop entry
//! twice bring the panel forward instead of starting a rival daemon.

pub mod protocol;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

pub use protocol::{ItemSummary, Request, Response, StatusReport};

use crate::paths;

/// How long the CLI waits for the daemon to answer.
///
/// Generous enough for a daemon busy writing a 40 MB clipping, short enough
/// that `clipboard-manager status` in a script cannot hang a terminal.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

// ---- single instance --------------------------------------------------------

/// An held `flock` proving we are the only daemon for this session.
///
/// The file descriptor must stay open for the lock to hold, which is the whole
/// reason this type exists rather than a bare function.
#[derive(Debug)]
pub struct InstanceLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl InstanceLock {
    /// Take the lock, or report who already has it.
    pub fn acquire() -> Result<Self> {
        let dir = paths::ipc_dir();
        paths::ensure_private_dir(&dir)
            .with_context(|| format!("could not prepare {}", dir.display()))?;

        let path = paths::ipc_lock();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;

        // SAFETY: `file` owns a valid descriptor for the duration of the call.
        // LOCK_NB so we fail immediately rather than blocking behind the
        // running daemon.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                bail!("another clipboard-manager is already running for this session");
            }
            return Err(err).context("could not lock the instance file");
        }

        Ok(Self { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---- server -----------------------------------------------------------------

/// A request paired with the channel its answer must go back on.
///
/// The accept loop lives on its own thread but the daemon's state lives on the
/// GTK main thread, so the request crosses over and the reply crosses back.
pub struct Pending {
    pub request: Request,
    reply: std::sync::mpsc::Sender<Response>,
}

impl Pending {
    /// Answer the request. Dropping a `Pending` without answering sends a
    /// generic error instead of hanging the client.
    pub fn reply(self, response: Response) {
        // The client may have gone away mid-request, which is routine.
        let _ = self.reply.send(response);
    }
}

/// The listening socket.
pub struct Server {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    requests: async_channel::Receiver<Pending>,
}

impl Server {
    /// Bind the socket and start accepting.
    ///
    /// A stale socket from a previous run is removed first — the `flock` has
    /// already proved no daemon is alive, so an existing socket file can only
    /// be debris from a crash.
    pub fn start() -> Result<Self> {
        let dir = paths::ipc_dir();
        paths::ensure_private_dir(&dir)?;

        let path = paths::ipc_socket();
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("could not remove the stale socket {}", path.display()))?;
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("could not bind {}", path.display()))?;
        // Belt and braces: the directory is already 0700, but a socket anyone
        // could connect to would be a §28 violation if that ever changed.
        paths::secure_file(&path)?;

        let (tx, rx) = async_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));

        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("clip-ipc".to_string())
            .spawn(move || accept_loop(listener, tx, thread_stop))
            .context("could not start the IPC thread")?;

        tracing::info!(socket = %path.display(), "listening for commands");
        Ok(Self { path, stop, thread: Some(thread), requests: rx })
    }

    /// Requests waiting to be handled on the main thread.
    pub fn requests(&self) -> async_channel::Receiver<Pending> {
        self.requests.clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock the accept by connecting to ourselves. Closing the listener
        // would be cleaner but `UnixListener` has no shutdown, and a self
        // connection is cheap and certain.
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn accept_loop(
    listener: UnixListener,
    requests: async_channel::Sender<Pending>,
    stop: Arc<AtomicBool>,
) {
    for incoming in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match incoming {
            Ok(stream) => {
                if let Err(err) = serve_one(stream, &requests) {
                    // One malformed client must not stop the daemon answering
                    // the next (§35).
                    tracing::debug!(error = %err, "dropped an IPC connection");
                }
            }
            Err(err) => {
                tracing::debug!(error = %err, "IPC accept failed");
                if stop.load(Ordering::Relaxed) {
                    return;
                }
            }
        }
    }
}

fn serve_one(stream: UnixStream, requests: &async_channel::Sender<Pending>) -> Result<()> {
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();

    // `take` bounds what a local process can make us allocate.
    let read = (&mut reader)
        .take(protocol::MAX_LINE_BYTES as u64)
        .read_line(&mut line)?;
    if read == 0 {
        // The self-connect used to unblock `accept` on shutdown lands here.
        return Ok(());
    }

    let mut writer = stream;
    let response = match Request::parse(line.trim()) {
        Ok(request) => {
            let (tx, rx) = std::sync::mpsc::channel();
            if requests.send_blocking(Pending { request, reply: tx }).is_err() {
                Response::error("the daemon is shutting down")
            } else {
                // Bounded wait: if the main thread is wedged, the client gets an
                // error rather than blocking forever.
                rx.recv_timeout(CLIENT_TIMEOUT)
                    .unwrap_or_else(|_| Response::error("the daemon did not respond in time"))
            }
        }
        Err(message) => Response::error(message),
    };

    writeln!(writer, "{}", response.encode())?;
    writer.flush()?;
    Ok(())
}

// ---- client -----------------------------------------------------------------

/// Send one request to a running daemon.
///
/// Returns `Err` when no daemon is listening, which the CLI distinguishes from
/// a daemon that answered with an error.
pub fn send(request: &Request) -> Result<Response> {
    let path = paths::ipc_socket();
    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "no clipboard-manager is running for this session (no socket at {})",
            path.display()
        )
    })?;

    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;

    let mut writer = stream.try_clone()?;
    writeln!(writer, "{}", request.encode()).context("could not send the request")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = (&mut reader)
        .take(protocol::MAX_LINE_BYTES as u64)
        .read_line(&mut line)
        .context("could not read the reply")?;
    if read == 0 {
        bail!("the daemon closed the connection without replying");
    }

    Response::parse(line.trim()).map_err(|message| anyhow::anyhow!(message))
}

/// Is a daemon already running for this session?
pub fn is_running() -> bool {
    matches!(send(&Request::Ping), Ok(response) if !response.is_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the XDG runtime directory at a temporary one, so these tests
    /// cannot touch a real session's socket.
    struct Sandbox {
        _dir: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl Sandbox {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("XDG_RUNTIME_DIR");
            // Safe here: the IPC tests are serialised by `SOCKET` below.
            std::env::set_var("XDG_RUNTIME_DIR", dir.path());
            Self { _dir: dir, previous }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("XDG_RUNTIME_DIR", value),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    /// These tests share one process-wide environment variable and one socket
    /// path, so they must not run concurrently.
    static SOCKET: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        SOCKET.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn a_second_instance_cannot_take_the_lock() {
        let _guard = guard();
        let _sandbox = Sandbox::new();

        let first = InstanceLock::acquire().expect("the first lock should succeed");
        let second = InstanceLock::acquire();
        assert!(second.is_err(), "a second daemon must be refused");
        assert!(second.unwrap_err().to_string().contains("already running"));

        // Releasing it must let the next one in, so a restart works.
        drop(first);
        assert!(InstanceLock::acquire().is_ok());
    }

    #[test]
    fn the_lock_file_lives_under_the_runtime_directory() {
        let _guard = guard();
        let sandbox = Sandbox::new();
        let lock = InstanceLock::acquire().unwrap();
        assert!(
            lock.path().starts_with(sandbox._dir.path()),
            "{} should be under {}",
            lock.path().display(),
            sandbox._dir.path().display()
        );
    }

    #[test]
    fn a_request_reaches_the_server_and_the_reply_comes_back() {
        let _guard = guard();
        let _sandbox = Sandbox::new();

        let server = Server::start().expect("the server should bind");
        let requests = server.requests();

        // Stand in for the GTK main thread.
        let handler = std::thread::spawn(move || {
            let pending = requests.recv_blocking().expect("a request should arrive");
            assert_eq!(pending.request, Request::Ping);
            pending.reply(Response::message("pong"));
        });

        let response = send(&Request::Ping).expect("the client should get a reply");
        assert_eq!(response, Response::message("pong"));
        handler.join().unwrap();
    }

    #[test]
    fn the_socket_is_not_readable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = guard();
        let _sandbox = Sandbox::new();

        let server = Server::start().unwrap();
        let mode = std::fs::metadata(server.path()).unwrap().permissions().mode() & 0o777;
        // §28: no group or other access to anything carrying clipboard data.
        assert_eq!(mode & 0o077, 0, "socket mode is {mode:#o}");
    }

    #[test]
    fn a_malformed_request_gets_an_error_rather_than_killing_the_server() {
        let _guard = guard();
        let _sandbox = Sandbox::new();

        let server = Server::start().unwrap();
        let requests = server.requests();

        // 1. A bad line is rejected by the IPC thread itself.
        let path = paths::ipc_socket();
        let mut stream = UnixStream::connect(&path).unwrap();
        writeln!(stream, "{{\"op\":\"not_a_real_op\"}}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert!(Response::parse(line.trim()).unwrap().is_error(), "{line}");

        // 2. It was never forwarded to the main thread. Checked only after the
        // reply has been read, so there is nothing still in flight to race
        // against.
        assert!(requests.is_empty(), "a malformed request must not be forwarded");

        // 3. The server is still alive and still serving.
        let handler = std::thread::spawn(move || {
            let pending = requests.recv_blocking().unwrap();
            assert_eq!(pending.request, Request::Toggle);
            pending.reply(Response::ok());
        });
        assert!(!send(&Request::Toggle).unwrap().is_error());
        handler.join().unwrap();
    }

    #[test]
    fn no_daemon_means_a_clear_error_not_a_hang() {
        let _guard = guard();
        let _sandbox = Sandbox::new();

        let err = send(&Request::Status).unwrap_err().to_string();
        assert!(err.contains("no clipboard-manager is running"), "{err}");
        assert!(!is_running());
    }

    #[test]
    fn a_stale_socket_file_does_not_stop_the_daemon_binding() {
        let _guard = guard();
        let _sandbox = Sandbox::new();

        // Simulate debris from a crash: a socket file with nothing behind it.
        let dir = paths::ipc_dir();
        paths::ensure_private_dir(&dir).unwrap();
        std::fs::write(paths::ipc_socket(), b"").unwrap();

        let server = Server::start();
        assert!(server.is_ok(), "{:?}", server.err());
    }
}
