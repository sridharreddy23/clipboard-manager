//! A self-pipe for interrupting a backend parked in `poll(2)`.
//!
//! Both monitoring backends block on their display-server socket and must be
//! able to return promptly when the daemon shuts down (§35, §45). Neither X11
//! nor Wayland offers a way to inject a synthetic event from another thread
//! without taking a lock the event loop may already hold, so the standard
//! self-pipe trick is used instead: the backend waits on its socket *and* on
//! the read end of a pipe, and [`MonitorContext::set_waker`] is handed a
//! closure that writes one byte to the write end.
//!
//! [`MonitorContext::set_waker`]: crate::clipboard::MonitorContext::set_waker

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Both ends of a non-blocking pipe, kept in one allocation.
///
/// Holding the *read* end alongside the write end is deliberate. A waker
/// closure can outlive the backend that registered it — the supervisor replaces
/// the closure only when the next backend starts — and writing to a pipe whose
/// read end has been closed raises `SIGPIPE`, whose default disposition would
/// take the whole process down. Keeping both ends behind the same [`Arc`] makes
/// that impossible to express.
#[derive(Debug)]
pub struct WakePipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl WakePipe {
    pub fn new() -> Result<Arc<Self>> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a two-element array of the required type, which is
        // the only precondition `pipe2` has.
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if rc != 0 {
            return Err(io::Error::last_os_error()).context("could not create a wake-up pipe");
        }
        // SAFETY: `pipe2` returned successfully, so both descriptors are fresh
        // and owned by us.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Ok(Arc::new(Self { read, write }))
    }

    /// The descriptor to include in a `poll(2)` set.
    pub fn read_fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }

    /// Interrupt a waiter. Safe to call from any thread, repeatedly, and after
    /// the backend has already exited.
    pub fn wake(&self) {
        let byte = 1u8;
        // SAFETY: a one-byte write from a valid local buffer to a descriptor we
        // own. The pipe is non-blocking, so a full pipe returns `EAGAIN` rather
        // than blocking, and a full pipe already means a wake-up is pending.
        let _ = unsafe {
            libc::write(self.write.as_raw_fd(), std::ptr::addr_of!(byte).cast(), 1)
        };
    }

    /// Discard pending wake-up bytes so the next wait blocks again.
    pub fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: a read of at most `buf.len()` bytes into a valid local
            // buffer from a descriptor we own.
            let n = unsafe {
                libc::read(self.read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
            };
            if n <= 0 {
                return;
            }
        }
    }
}

/// Which descriptor became ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// The display-server socket has data.
    Socket,
    /// [`WakePipe::wake`] was called.
    Woken,
    /// The deadline passed with nothing ready.
    TimedOut,
}

/// Block until `socket` is readable, `wake` is signalled, or `deadline` passes.
///
/// `deadline` of `None` waits indefinitely. `EINTR` is handled internally by
/// retrying with a recomputed timeout, so a signal cannot be mistaken for a
/// timeout.
pub fn wait(
    socket: BorrowedFd<'_>,
    wake: BorrowedFd<'_>,
    deadline: Option<Instant>,
) -> Result<Ready> {
    loop {
        let timeout_ms = match deadline {
            None => -1,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining == Duration::ZERO {
                    return Ok(Ready::TimedOut);
                }
                remaining.as_millis().min(i32::MAX as u128) as libc::c_int
            }
        };

        let mut fds = [
            libc::pollfd { fd: socket.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];

        // SAFETY: `fds` is a valid array of two `pollfd`s, and the count passed
        // matches its length. The borrowed descriptors are valid for the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };

        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("poll on the display-server socket failed");
        }
        if rc == 0 {
            return Ok(Ready::TimedOut);
        }

        // The wake-up is checked first so that shutdown is never starved by a
        // socket that is permanently readable.
        if fds[1].revents != 0 {
            return Ok(Ready::Woken);
        }
        if fds[0].revents != 0 {
            return Ok(Ready::Socket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wake_up_interrupts_a_wait() {
        let pipe = WakePipe::new().unwrap();
        let other = WakePipe::new().unwrap();

        let waker = Arc::clone(&pipe);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.wake();
        });

        let ready = wait(other.read_fd(), pipe.read_fd(), None).unwrap();
        assert_eq!(ready, Ready::Woken);
        pipe.drain();
    }

    #[test]
    fn a_deadline_that_has_passed_returns_immediately() {
        let a = WakePipe::new().unwrap();
        let b = WakePipe::new().unwrap();
        let deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(wait(a.read_fd(), b.read_fd(), Some(deadline)).unwrap(), Ready::TimedOut);
    }

    #[test]
    fn a_short_deadline_times_out_rather_than_hanging() {
        let a = WakePipe::new().unwrap();
        let b = WakePipe::new().unwrap();
        let started = Instant::now();
        let ready = wait(a.read_fd(), b.read_fd(), Some(started + Duration::from_millis(30)))
            .unwrap();
        assert_eq!(ready, Ready::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn draining_restores_the_blocking_behaviour() {
        let pipe = WakePipe::new().unwrap();
        let other = WakePipe::new().unwrap();

        pipe.wake();
        pipe.wake();
        assert_eq!(wait(other.read_fd(), pipe.read_fd(), None).unwrap(), Ready::Woken);

        pipe.drain();
        let deadline = Instant::now() + Duration::from_millis(20);
        assert_eq!(
            wait(other.read_fd(), pipe.read_fd(), Some(deadline)).unwrap(),
            Ready::TimedOut
        );
    }

    #[test]
    fn waking_after_the_waiter_has_gone_is_harmless() {
        // The whole point of holding both ends: this must not raise SIGPIPE.
        let pipe = WakePipe::new().unwrap();
        for _ in 0..10_000 {
            pipe.wake();
        }
        pipe.drain();
        pipe.wake();
    }

    #[test]
    fn socket_readiness_is_reported() {
        let socket = WakePipe::new().unwrap();
        let wake = WakePipe::new().unwrap();
        socket.wake();
        assert_eq!(wait(socket.read_fd(), wake.read_fd(), None).unwrap(), Ready::Socket);
    }
}
