//! Wayland clipboard monitoring through `wlr-data-control` (§19).
//!
//! # Why this protocol and no other
//!
//! Wayland deliberately gives an ordinary client no way to observe the
//! clipboard: a client may only read a selection it has been *offered*, which
//! happens when it holds keyboard focus. That is a security property, not a
//! defect, and it is why a clipboard manager needs an explicit protocol.
//!
//! `zwlr_data_control_manager_v1` is that protocol. A client binds it, gets a
//! data device for a seat, and from then on the compositor sends a
//! `selection` event — with the full MIME type list — every time the clipboard
//! changes, regardless of focus. Monitoring is therefore entirely event-driven:
//! this module blocks in `poll(2)` on the Wayland socket and does no polling of
//! its own.
//!
//! It is implemented by wlroots-based compositors (Sway, Hyprland, Wayfire,
//! river) and by KWin since Plasma 5.25. Mutter implements nothing equivalent,
//! so GNOME Wayland sessions are served by the X11 backend over XWayland
//! instead; see [`crate::platform`] for why that works and what it costs.
//!
//! # What this backend cannot do
//!
//! The protocol carries no information about which client owns a selection.
//! There is no window, no PID and no app ID in any of its events. So the
//! application exclusion list (§16) cannot be applied here, and
//! [`ClipboardBackend::caveat`] says so rather than letting the setting appear
//! to work. Password managers are still handled, because they announce
//! themselves through the `x-kde-passwordManagerHint` MIME type, which this
//! backend does see and [`crate::privacy`] does check.
//!
//! # Transfers
//!
//! Data arrives over a pipe: we hand the compositor a write end, the source
//! client writes the requested MIME type into it and closes it, and we read to
//! EOF. Both halves of that have to be guarded, so the read end is
//! non-blocking with an idle deadline and an overall deadline, and a source
//! that stops writing part-way costs one clipping rather than the monitor.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::EventQueue;
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};

use crate::clipboard::{ClipboardBackend, MonitorContext, Rejection};
use crate::model::{RawClip, Selection};
use crate::platform::wake::{self, Ready, WakePipe};

/// Name reported in logs and in `clipboard-manager status`.
pub const BACKEND_NAME: &str = "wayland-data-control";

/// The global we need. Named here rather than inline so [`probe`] and [`build`]
/// cannot disagree about it.
const MANAGER_INTERFACE: &str = "zwlr_data_control_manager_v1";

/// Longest gap we tolerate between two writes from the source client.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on one transfer, so a source that trickles bytes forever cannot hold
/// up monitoring indefinitely.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

// An idle gap must not be able to outlive the whole transfer, or the overall
// ceiling would never be the thing that fires.
const _: () = assert!(IDLE_TIMEOUT.as_secs() < TRANSFER_TIMEOUT.as_secs());

/// Read buffer size. Matches the usual pipe capacity, so a source that fills
/// the pipe is emptied in one call.
const READ_CHUNK: usize = 64 * 1024;

/// Upper bound on a single clipping, applied even when the user has chosen
/// "Unlimited".
///
/// The bytes are buffered in memory before being hashed and written, so an
/// unbounded transfer would mean an unbounded allocation. Documented in the
/// README's limitations section.
const ABSOLUTE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

// Enforced at compile time rather than by a test: a test comparing two
// constants is folded away and asserts nothing, whereas this fails the build.
// Guards against the cap being "tidied" down to a value that would silently
// break the Unlimited setting for ordinary content.
const _: () = assert!(ABSOLUTE_MAX_BYTES >= 1024 * 1024 * 1024);

/// Is this compositor able to serve clipboard history?
///
/// Connects, reads the global registry and looks for the manager. This is a
/// real check, not an inference from `XDG_CURRENT_DESKTOP`: §56 forbids
/// claiming a platform feature works when it does not, and a compositor that
/// *usually* has the protocol may have been built without it.
pub fn probe() -> Result<bool> {
    let conn = Connection::connect_to_env().context("could not connect to the Wayland display")?;
    let (globals, _queue) = registry_queue_init::<Probe>(&conn)
        .context("could not read the Wayland global registry")?;

    let found = globals
        .contents()
        .with_list(|list| list.iter().any(|global| global.interface == MANAGER_INTERFACE));
    Ok(found)
}

pub fn build() -> Result<Box<dyn ClipboardBackend>> {
    Ok(Box::new(WaylandBackend::connect()?))
}

/// Registry-only state used by [`probe`]. It never dispatches anything.
struct Probe;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// A selection change waiting to be retrieved.
///
/// Transfers are *queued* rather than performed inside the dispatch callback.
/// Reading a pipe from inside a callback would mean blocking with the event
/// queue half-dispatched, and a selection that is superseded before we get to
/// it would still be fetched. Queueing makes both problems go away: superseded
/// jobs are dropped in [`State::selection`] before any data is requested.
struct Job {
    selection: Selection,
    offer: ZwlrDataControlOfferV1,
    mimes: Vec<String>,
}

#[derive(Default)]
struct State {
    /// MIME types accumulated per offer, between the `data_offer` event and the
    /// `selection` event that gives the offer a meaning.
    mimes: HashMap<ObjectId, Vec<String>>,
    jobs: VecDeque<Job>,
    /// The offer currently describing each selection. Held because the protocol
    /// requires the client to destroy the previous offer when a new one
    /// arrives, and because an offer must stay alive to be read.
    clipboard: Option<ZwlrDataControlOfferV1>,
    primary: Option<ZwlrDataControlOfferV1>,
    /// Set when the compositor withdraws the data device.
    finished: bool,
}

impl State {
    fn slot(&mut self, selection: Selection) -> &mut Option<ZwlrDataControlOfferV1> {
        match selection {
            Selection::Clipboard => &mut self.clipboard,
            Selection::Primary => &mut self.primary,
        }
    }

    /// Handle a `selection` or `primary_selection` event.
    fn selection(&mut self, selection: Selection, offer: Option<ZwlrDataControlOfferV1>) {
        // Anything still queued for this selection has been superseded. Dropping
        // it now means we never spend a transfer on a clipping the user has
        // already replaced.
        while let Some(index) = self.jobs.iter().position(|job| job.selection == selection) {
            let job = self.jobs.remove(index).expect("index came from position");
            self.forget(&job.offer);
        }

        if let Some(previous) = self.slot(selection).take() {
            self.forget(&previous);
        }

        let Some(offer) = offer else {
            // The selection was cleared. History deliberately keeps what it
            // already has: a cleared clipboard is not a new clipping.
            tracing::debug!(selection = selection.as_str(), "selection cleared");
            return;
        };

        let mimes = self.mimes.remove(&offer.id()).unwrap_or_default();
        *self.slot(selection) = Some(offer.clone());
        self.jobs.push_back(Job { selection, offer, mimes });
    }

    /// Destroy an offer and forget its MIME list.
    fn forget(&mut self, offer: &ZwlrDataControlOfferV1) {
        self.mimes.remove(&offer.id());
        if offer.is_alive() {
            offer.destroy();
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            // The offer's MIME types arrive next, before the `selection` event
            // that tells us what the offer is for.
            Event::DataOffer { id } => {
                state.mimes.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => state.selection(Selection::Clipboard, id),
            Event::PrimarySelection { id } => state.selection(Selection::Primary, id),
            Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            // An offer whose entry has gone was superseded while its MIME types
            // were still arriving; ignoring it is correct.
            if let Some(mimes) = state.mimes.get_mut(&offer.id()) {
                // Bounded: the list comes from another client.
                if mimes.len() < 256 {
                    mimes.push(mime_type);
                }
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ZwlrDataControlManagerV1);

struct WaylandBackend {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    device: ZwlrDataControlDeviceV1,
    /// Kept alive for the lifetime of the device.
    _manager: ZwlrDataControlManagerV1,
    _seat: wl_seat::WlSeat,
    wake: Arc<WakePipe>,
    /// Whether the compositor negotiated version 2, which is what adds the
    /// primary-selection events.
    primary_available: bool,
}

impl WaylandBackend {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("could not connect to the Wayland display")?;
        let (globals, queue) = registry_queue_init::<State>(&conn)
            .context("could not read the Wayland global registry")?;
        let qh = queue.handle();

        // Version 2 adds primary-selection support. Asking for 1..=2 means a
        // version-1 compositor still works, with primary selection reported as
        // unavailable rather than silently broken.
        let manager: ZwlrDataControlManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .with_context(|| format!("this compositor does not provide {MANAGER_INTERFACE}"))?;

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=9, ())
            .context("this compositor provides no seat to read the clipboard of")?;

        let device = manager.get_data_device(&seat, &qh, ());
        let primary_available = manager.version() >= 2;

        tracing::info!(
            version = manager.version(),
            primary = primary_available,
            "watching the Wayland clipboard through wlr-data-control"
        );

        Ok(Self {
            conn,
            queue,
            state: State::default(),
            device,
            _manager: manager,
            _seat: seat,
            wake: WakePipe::new()?,
            primary_available,
        })
    }

    /// Perform one queued transfer.
    fn process(&mut self, job: Job, ctx: &MonitorContext) {
        if !ctx.interested(job.selection) {
            return;
        }
        let Some(chosen) = ctx.wants(job.selection, &job.mimes, None) else {
            return;
        };

        let settings = ctx.settings();
        let limit = settings
            .clipboard
            .max_item_bytes()
            .unwrap_or(ABSOLUTE_MAX_BYTES)
            .min(ABSOLUTE_MAX_BYTES);

        match self.receive(&job.offer, &chosen.mime, limit, ctx) {
            Ok(Some(bytes)) => ctx.submit(RawClip {
                offered: job.mimes,
                mime: chosen.mime,
                bytes,
                selection: job.selection,
                source_app: None,
            }),
            Ok(None) => {}
            // One failed transfer must never stop monitoring (§35). A genuinely
            // broken connection surfaces on the next dispatch, where it does
            // propagate and the supervisor reconnects.
            Err(err) => tracing::debug!(
                selection = job.selection.as_str(),
                error = %err,
                "could not read the selection"
            ),
        }
    }

    /// Ask the source client for `mime` and read the pipe to EOF.
    fn receive(
        &self,
        offer: &ZwlrDataControlOfferV1,
        mime: &str,
        limit: u64,
        ctx: &MonitorContext,
    ) -> Result<Option<Vec<u8>>> {
        if !offer.is_alive() {
            // Superseded between queueing and here.
            return Ok(None);
        }

        let (read, write) = pipe()?;
        offer.receive(mime.to_string(), write.as_fd());
        // The request has to reach the compositor before anyone writes to the
        // pipe, and nothing else in this loop flushes between here and the read.
        self.conn.flush().context("could not flush the receive request")?;
        // Our copy of the write end must go, or EOF never arrives.
        drop(write);

        let overall = Instant::now() + TRANSFER_TIMEOUT;
        let mut data: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; READ_CHUNK];

        loop {
            // SAFETY: a read of at most `buf.len()` bytes into a valid local
            // buffer from a descriptor we own.
            let n = unsafe {
                libc::read(read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
            };

            if n > 0 {
                let n = n as usize;
                if data.len() as u64 + n as u64 > limit {
                    // Closing the read end makes the source's next write fail
                    // with EPIPE, which is the ordinary "receiver went away"
                    // path every clipboard implementation already handles. The
                    // size reported is a lower bound: we stopped counting here.
                    ctx.reject(&Rejection::TooLarge { size: data.len() as u64 + n as u64, limit });
                    return Ok(None);
                }
                data.extend_from_slice(&buf[..n]);
                continue;
            }
            if n == 0 {
                // EOF: the source finished and closed its end.
                return Ok(Some(data));
            }

            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {}
                _ => return Err(err).context("reading the clipboard pipe failed"),
            }

            // Nothing available yet. Wait for the source, for shutdown, or for
            // the deadline — whichever comes first.
            let deadline = overall.min(Instant::now() + IDLE_TIMEOUT);
            match wake::wait(read.as_fd(), self.wake.read_fd(), Some(deadline))? {
                Ready::Socket => {}
                Ready::Woken => {
                    self.wake.drain();
                    if ctx.should_stop() {
                        return Ok(None);
                    }
                }
                Ready::TimedOut => {
                    if Instant::now() >= overall {
                        bail!("the transfer exceeded {TRANSFER_TIMEOUT:?}");
                    }
                    bail!("the source client stopped writing for {IDLE_TIMEOUT:?}");
                }
            }
        }
    }
}

impl ClipboardBackend for WaylandBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn caveat(&self) -> Option<String> {
        let mut caveat = String::from(
            "Wayland does not identify the application that owns a selection, so \
             the application exclusion list cannot be applied.",
        );
        if !self.primary_available {
            caveat.push_str(
                " This compositor implements version 1 of wlr-data-control, which \
                 cannot report the primary (middle-click) selection.",
            );
        }
        Some(caveat)
    }

    fn run(&mut self, ctx: &MonitorContext) -> Result<()> {
        let wake = Arc::clone(&self.wake);
        ctx.set_waker(Box::new(move || wake.wake()));

        // Binding the device makes the compositor send a `selection` event for
        // the current clipboard straight away, so a restart does not lose the
        // entry the user copied a moment ago.
        self.conn.flush()?;

        loop {
            if ctx.should_stop() {
                return Ok(());
            }

            self.queue
                .dispatch_pending(&mut self.state)
                .context("dispatching Wayland events failed")?;

            if self.state.finished {
                // Returning an error is deliberate: the supervisor's job is to
                // rebuild the backend, and a withdrawn data device is exactly
                // the case where reconnecting is the right answer.
                bail!("the compositor withdrew the data-control device");
            }

            if let Some(job) = self.state.jobs.pop_front() {
                self.process(job, ctx);
                continue;
            }

            self.conn.flush()?;

            // The guard must exist before we poll, or a concurrent reader could
            // consume the events between the poll and the read and leave us
            // sleeping on an empty socket. `None` means events are already
            // queued, so there is nothing to wait for.
            let Some(guard) = self.conn.prepare_read() else {
                continue;
            };

            match wake::wait(guard.connection_fd(), self.wake.read_fd(), None)? {
                Ready::Socket => match guard.read() {
                    Ok(_) => {}
                    // Another thread got there first, or a spurious wake-up.
                    Err(WaylandError::Io(err)) if err.kind() == io::ErrorKind::WouldBlock => {}
                    Err(err) => {
                        return Err(err).context("reading from the Wayland socket failed")
                    }
                },
                Ready::Woken => {
                    drop(guard);
                    self.wake.drain();
                }
                Ready::TimedOut => drop(guard),
            }
        }
    }
}

impl Drop for WaylandBackend {
    fn drop(&mut self) {
        // Destroy the offers we still hold, then the device, so the compositor
        // can release its side immediately rather than at socket close.
        for offer in [self.state.clipboard.take(), self.state.primary.take()].into_iter().flatten()
        {
            if offer.is_alive() {
                offer.destroy();
            }
        }
        if self.device.is_alive() {
            self.device.destroy();
        }
        let _ = self.conn.flush();
    }
}

/// A `CLOEXEC` pipe whose read end is non-blocking.
///
/// The write end stays blocking: it is handed to another process, which is
/// entitled to expect ordinary pipe semantics.
fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a two-element array of the required type, which is the
    // only precondition `pipe2` has.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(io::Error::last_os_error()).context("could not create a transfer pipe");
    }
    // SAFETY: `pipe2` returned successfully, so both descriptors are fresh and
    // owned by us.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

    // SAFETY: `read` is a valid descriptor we own; `F_SETFL` with `O_NONBLOCK`
    // is the documented way to make it non-blocking.
    let rc = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(io::Error::last_os_error())
            .context("could not make the transfer pipe non-blocking");
    }

    Ok((read, write))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_pipe_is_non_blocking_on_the_read_side() {
        let (read, _write) = pipe().unwrap();
        let mut buf = [0u8; 4];
        let n = unsafe { libc::read(read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert_eq!(n, -1);
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn a_transfer_pipe_carries_data_and_signals_eof() {
        let (read, write) = pipe().unwrap();
        let payload = b"ffmpeg -i input.mp4 output.ts";
        let n = unsafe {
            libc::write(write.as_raw_fd(), payload.as_ptr().cast(), payload.len())
        };
        assert_eq!(n, payload.len() as isize);
        drop(write);

        let mut buf = [0u8; 64];
        let n = unsafe { libc::read(read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert_eq!(&buf[..n as usize], payload);

        // Zero, not EAGAIN: the write end is closed, so this is a real EOF.
        let n = unsafe { libc::read(read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert_eq!(n, 0);
    }

    #[test]
    fn the_probed_interface_name_matches_the_bound_type() {
        assert_eq!(ZwlrDataControlManagerV1::interface().name, MANAGER_INTERFACE);
    }
}
