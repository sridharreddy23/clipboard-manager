//! X11 clipboard monitoring (§18).
//!
//! # How a clipping is captured
//!
//! X11 has no "clipboard". It has *selections*: an atom (`CLIPBOARD`,
//! `PRIMARY`) owned by a client, which other clients ask to convert into a
//! *target* (also an atom, in practice a MIME type name). Nothing is stored by
//! the server — the owner serves the data on request, which is why clipboard
//! content disappears when the owning application exits.
//!
//! Capture therefore works like this:
//!
//! 1. The XFIXES extension tells us when a selection changes owner. This is
//!    what makes monitoring event-driven: `XFixesSelectSelectionInput` asks the
//!    server to send us a `SelectionNotify` whenever the owner is set, so an
//!    idle session costs nothing. There is no polling anywhere in this file.
//! 2. We ask the new owner to convert the selection to `TARGETS`, which yields
//!    the list of types it can produce.
//! 3. [`MonitorContext::wants`] picks one, or declines — privacy decisions
//!    happen here, *before* any payload is requested.
//! 4. We ask for that one target, read it out of a property on our own
//!    (unmapped, 1×1) window, and hand the bytes to the pipeline.
//!
//! Step 4 handles the `INCR` protocol, which is how X11 transfers anything
//! larger than the server's maximum request size: the owner replies with an
//! `INCR` property containing a size estimate, and then streams the real data
//! in chunks, each announced by a `PropertyNotify` and acknowledged by us
//! deleting the property. A 40 MB screenshot arrives this way.
//!
//! # PRIMARY
//!
//! `PRIMARY` — the middle-click selection, which changes on every drag of the
//! mouse over text — is watched but ignored unless the user opts in, because
//! recording it would bury the real clipboard history in noise. The decision
//! lives in [`MonitorContext::interested`], not here.
//!
//! # Robustness
//!
//! Every wait has a deadline, so a selection owner that advertises a target and
//! then never answers cannot wedge the monitor; the clipping is dropped and
//! monitoring continues (§35). Selection changes that arrive while a transfer
//! is in progress are stashed and processed afterwards rather than discarded.

mod source_app;

use std::collections::{HashMap, VecDeque};
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Property, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{atom_manager, COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

use crate::clipboard::{ClipboardBackend, MonitorContext, Rejection};
use crate::model::{RawClip, Selection};
use crate::platform::wake::{self, Ready, WakePipe};

/// Name reported in logs and in `clipboard-manager status`.
pub const BACKEND_NAME: &str = "x11";

/// How long to wait for a selection owner to answer a conversion request.
///
/// Generous enough for an application that has to generate the data (a
/// spreadsheet serialising a selection), short enough that a hung application
/// costs one clipping rather than all future ones.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the next chunk of an `INCR` transfer. Reset per chunk,
/// so a legitimately slow 100 MB transfer is not cut off part-way.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(10);

/// Overall ceiling on one `INCR` transfer, so a peer that keeps sending
/// one-byte chunks cannot occupy the monitor forever.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

// Ordered so that a per-chunk stall fires before the overall ceiling, and a
// missing first reply fires before either.
const _: () = assert!(CHUNK_TIMEOUT.as_secs() < TRANSFER_TIMEOUT.as_secs());
const _: () = assert!(REPLY_TIMEOUT.as_secs() <= CHUNK_TIMEOUT.as_secs());

/// Bytes requested per `GetProperty` round trip (expressed in 32-bit words, as
/// the protocol requires). 256 KiB keeps the number of round trips for a large
/// image small without building an enormous reply.
const CHUNK_WORDS: u32 = 64 * 1024;

// The maximum length of an X11 request without BIG-REQUESTS is 2^16 words, and
// a GetProperty reply must fit the server's reply limit; 64 Ki words (256 KiB)
// is the conventional safe chunk.
const _: () = assert!(CHUNK_WORDS <= 64 * 1024);

/// Upper bound on a single clipping, applied even when the user has chosen
/// "Unlimited" for the size limit.
///
/// This is not a hidden second limit on normal use: it exists because the bytes
/// are buffered in memory before being hashed and written, and an application
/// that advertises a 3 GB selection would otherwise get the daemon OOM-killed.
/// It is documented in the README's limitations section.
const ABSOLUTE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

// Compile-time, so a bad edit fails the build rather than a folded-away test.
const _: () = assert!(ABSOLUTE_MAX_BYTES >= 1024 * 1024 * 1024);

/// Cap on events set aside while waiting for a transfer to complete. Reaching
/// it would need hundreds of selection changes inside one transfer window.
const MAX_STASH: usize = 256;

atom_manager! {
    /// Atoms interned once per connection.
    ///
    /// `RECV` is the property on our own helper window into which selection
    /// owners deliver data; the rest are standard.
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        INCR,
        UTF8_STRING,
        WM_CLASS,
        WM_CLIENT_MACHINE,
        _NET_WM_PID,
        _NET_ACTIVE_WINDOW,
        RECV: b"_CLIPBOARD_MANAGER_RECV",
    }
}

pub fn build() -> Result<Box<dyn ClipboardBackend>> {
    Ok(Box::new(X11Backend::connect()?))
}

struct X11Backend {
    conn: RustConnection,
    atoms: Atoms,
    /// Unmapped 1×1 window that owns the receiving property and receives
    /// `SelectionNotify` and `PropertyNotify`.
    window: Window,
    root: Window,
    wake: Arc<WakePipe>,
    /// Events read from the connection but not yet dispatched, including those
    /// set aside during a nested wait.
    deferred: VecDeque<Event>,
    /// Atom-name cache. Target lists repeat constantly, and `GetAtomName` is a
    /// round trip.
    names: HashMap<Atom, String>,
}

impl X11Backend {
    fn connect() -> Result<Self> {
        let (conn, screen_num) =
            x11rb::connect(None).context("could not connect to the X server")?;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .context("the X server reported no screens")?
            .root;

        let atoms = Atoms::new(&conn)?
            .reply()
            .context("could not intern the atoms we need")?;

        // XFIXES requires a version negotiation before any of its other
        // requests may be used. Selection notifications exist since version 1;
        // asking for 5.0 simply gets us the highest the server supports.
        let version = conn
            .xfixes_query_version(5, 0)
            .context("the X server does not provide the XFIXES extension")?
            .reply()
            .context("the X server does not provide the XFIXES extension")?;
        if version.major_version < 1 {
            bail!(
                "XFIXES {}.{} is too old to report selection changes; \
                 clipboard history needs XFIXES 1.0 or later",
                version.major_version,
                version.minor_version
            );
        }

        let window = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            // PropertyChange is what makes INCR transfers observable. The
            // window is never mapped, so it is invisible and takes no input.
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .context("could not create the helper window")?;

        let mask = SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE;
        // PRIMARY is registered unconditionally so that enabling it in settings
        // takes effect immediately; the events are discarded cheaply while it
        // is off.
        for selection in [atoms.CLIPBOARD, AtomEnum::PRIMARY.into()] {
            conn.xfixes_select_selection_input(window, selection, mask)
                .context("could not subscribe to selection changes")?;
        }
        conn.flush()?;

        tracing::info!(
            xfixes = format!("{}.{}", version.major_version, version.minor_version),
            "watching the X11 CLIPBOARD selection"
        );

        Ok(Self {
            conn,
            atoms,
            window,
            root,
            wake: WakePipe::new()?,
            deferred: VecDeque::new(),
            names: HashMap::new(),
        })
    }

    fn selection_atom(&self, selection: Selection) -> Atom {
        match selection {
            Selection::Clipboard => self.atoms.CLIPBOARD,
            Selection::Primary => AtomEnum::PRIMARY.into(),
        }
    }

    fn selection_from_atom(&self, atom: Atom) -> Option<Selection> {
        if atom == self.atoms.CLIPBOARD {
            Some(Selection::Clipboard)
        } else if atom == Atom::from(AtomEnum::PRIMARY) {
            Some(Selection::Primary)
        } else {
            None
        }
    }

    // -- event loop -----------------------------------------------------------

    /// Next event from the deferred queue, or from the connection if the queue
    /// is empty. Never blocks.
    fn take_event(&mut self) -> Result<Option<Event>> {
        if let Some(event) = self.deferred.pop_front() {
            return Ok(Some(event));
        }
        // Draining the connection's parsed queue before polling the socket is
        // required: a reply we awaited earlier may have buffered events, and
        // the socket would then not be readable even though events are pending.
        Ok(self.conn.poll_for_event()?)
    }

    /// Block until an event is available, the deadline passes, or shutdown is
    /// requested. `None` means "stop waiting", not "connection broken".
    fn next_event(
        &mut self,
        deadline: Option<Instant>,
        ctx: &MonitorContext,
    ) -> Result<Option<Event>> {
        loop {
            if let Some(event) = self.take_event()? {
                return Ok(Some(event));
            }
            if ctx.should_stop() {
                return Ok(None);
            }
            self.conn.flush()?;
            // Re-check after flushing: flushing can parse incoming events.
            if let Some(event) = self.take_event()? {
                return Ok(Some(event));
            }

            match wake::wait(self.conn.stream().as_fd(), self.wake.read_fd(), deadline)? {
                Ready::Socket => {}
                Ready::Woken => {
                    self.wake.drain();
                    if ctx.should_stop() {
                        return Ok(None);
                    }
                }
                Ready::TimedOut => return Ok(None),
            }
        }
    }

    /// Wait for an event matching `matches`, setting everything else aside for
    /// the main loop to handle once the transfer is over.
    ///
    /// Stashing rather than discarding matters: the events arriving here
    /// include selection changes, and dropping one would silently lose a
    /// clipping.
    fn wait_for<F>(
        &mut self,
        deadline: Instant,
        ctx: &MonitorContext,
        mut matches: F,
    ) -> Result<Option<Event>>
    where
        F: FnMut(&Event) -> bool,
    {
        let mut stash: Vec<Event> = Vec::new();
        let mut found = None;

        loop {
            match self.next_event(Some(deadline), ctx)? {
                Some(event) if matches(&event) => {
                    found = Some(event);
                    break;
                }
                Some(event) => {
                    if stash.len() < MAX_STASH {
                        stash.push(event);
                    } else {
                        tracing::warn!("too many events during a transfer; one was discarded");
                    }
                }
                None => break,
            }
        }

        // Put them back at the front, in order, so they are handled before
        // anything that arrives later.
        for event in stash.into_iter().rev() {
            self.deferred.push_front(event);
        }
        Ok(found)
    }

    fn dispatch(&mut self, event: Event, ctx: &MonitorContext) {
        match event {
            Event::XfixesSelectionNotify(event) => {
                let Some(selection) = self.selection_from_atom(event.selection) else {
                    return;
                };
                if event.subtype == xfixes::SelectionEvent::SET_SELECTION_OWNER {
                    self.capture(selection, event.owner, ctx);
                } else {
                    // The owner exited or destroyed its window, so the content
                    // is simply gone. Not an error: it is how X11 works, and
                    // the copy we already took is unaffected (§35).
                    tracing::debug!(
                        selection = selection.as_str(),
                        "selection owner went away"
                    );
                }
            }
            // `PropertyNotify` for our receive property outside a transfer, and
            // anything else the server sends us, is of no interest.
            _ => {}
        }
    }

    /// Read whatever is on the clipboard right now.
    ///
    /// Called when the backend starts, which includes after a reconnect: the
    /// selection may well have changed while we were not watching, and the
    /// entry the user copied a moment before the daemon started is exactly the
    /// one they are about to look for.
    fn capture_current(&mut self, ctx: &MonitorContext) {
        let selections = [Selection::Clipboard, Selection::Primary];
        for selection in selections {
            if !ctx.interested(selection) {
                continue;
            }
            let atom = self.selection_atom(selection);
            let owner = self
                .conn
                .get_selection_owner(atom)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| reply.owner)
                .unwrap_or(NONE);
            if owner != NONE {
                self.capture(selection, owner, ctx);
            }
        }
    }

    // -- capture --------------------------------------------------------------

    fn capture(&mut self, selection: Selection, owner: Window, ctx: &MonitorContext) {
        if owner == NONE || owner == self.window {
            return;
        }
        // Cheap gate, so PRIMARY churn costs no round trips while it is off.
        if !ctx.interested(selection) {
            return;
        }

        if let Err(err) = self.try_capture(selection, owner, ctx) {
            // One unreadable selection must never stop monitoring. A genuinely
            // broken connection surfaces on the next loop iteration, where it
            // does propagate and the supervisor reconnects (§35).
            tracing::debug!(
                selection = selection.as_str(),
                error = %err,
                "could not read the selection"
            );
        }
    }

    fn try_capture(
        &mut self,
        selection: Selection,
        owner: Window,
        ctx: &MonitorContext,
    ) -> Result<()> {
        let atom = self.selection_atom(selection);
        let app = source_app::identify(&self.conn, &self.atoms, owner, self.root);

        let targets = self.read_targets(atom, ctx)?;
        let offered: Vec<String> = targets.iter().map(|(_, name)| name.clone()).collect();

        let Some(chosen) = ctx.wants(selection, &offered, app.as_ref()) else {
            return Ok(());
        };
        let Some((target, _)) = targets
            .iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(&chosen.mime))
        else {
            // `wants` only ever returns a target from the list we gave it.
            bail!("chosen target {} vanished from the offer list", chosen.mime);
        };

        let settings = ctx.settings();
        let limit = settings
            .clipboard
            .max_item_bytes()
            .unwrap_or(ABSOLUTE_MAX_BYTES)
            .min(ABSOLUTE_MAX_BYTES);

        let Some(bytes) = self.convert(atom, *target, ctx, limit)? else {
            return Ok(());
        };

        ctx.submit(RawClip {
            offered,
            mime: chosen.mime,
            bytes,
            selection,
            source_app: app,
        });
        Ok(())
    }

    /// Ask the owner what it can produce.
    fn read_targets(
        &mut self,
        selection: Atom,
        ctx: &MonitorContext,
    ) -> Result<Vec<(Atom, String)>> {
        let targets = self.atoms.TARGETS;
        // A target list is a handful of atoms; the limit only guards against a
        // peer answering TARGETS with something absurd.
        let Some(bytes) = self.convert(selection, targets, ctx, 64 * 1024)? else {
            return Ok(self.fallback_targets());
        };
        if bytes.len() < 4 {
            return Ok(self.fallback_targets());
        }

        // Property values of format 32 arrive in the connection's byte order,
        // which x11rb negotiates as the native one at connection setup.
        let atoms: Vec<Atom> = bytes
            .chunks_exact(4)
            .map(|c| Atom::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .filter(|atom| *atom != NONE)
            .collect();

        Ok(self.resolve_names(&atoms))
    }

    /// Targets to assume for an owner that does not implement `TARGETS`.
    ///
    /// ICCCM requires every owner to support it, but a few old toolkits do not.
    /// Both of these are understood by every X11 client that handles text at
    /// all, so asking for them is safe.
    fn fallback_targets(&self) -> Vec<(Atom, String)> {
        tracing::debug!("owner does not implement TARGETS; assuming plain text");
        vec![
            (self.atoms.UTF8_STRING, "UTF8_STRING".to_string()),
            (AtomEnum::STRING.into(), "STRING".to_string()),
        ]
    }

    /// Turn atoms into names, batching the round trips and caching the results.
    fn resolve_names(&mut self, atoms: &[Atom]) -> Vec<(Atom, String)> {
        let unknown: Vec<Atom> = atoms
            .iter()
            .copied()
            .filter(|atom| !self.names.contains_key(atom))
            .collect();

        // Send every request before awaiting any reply, so a 30-target list
        // costs one round trip rather than thirty.
        let mut cookies = Vec::with_capacity(unknown.len());
        for atom in &unknown {
            match self.conn.get_atom_name(*atom) {
                Ok(cookie) => cookies.push((*atom, cookie)),
                Err(err) => {
                    tracing::debug!(error = %err, "could not request an atom name");
                    break;
                }
            }
        }
        for (atom, cookie) in cookies {
            if let Ok(reply) = cookie.reply() {
                if let Ok(name) = String::from_utf8(reply.name) {
                    self.names.insert(atom, name);
                }
            }
        }

        atoms
            .iter()
            .filter_map(|atom| self.names.get(atom).map(|name| (*atom, name.clone())))
            .collect()
    }

    /// Convert `selection` to `target` and return the bytes.
    ///
    /// `Ok(None)` means the owner declined, the data was over `limit`, or the
    /// wait was interrupted — all routine outcomes that leave monitoring
    /// running.
    fn convert(
        &mut self,
        selection: Atom,
        target: Atom,
        ctx: &MonitorContext,
        limit: u64,
    ) -> Result<Option<Vec<u8>>> {
        let window = self.window;
        let property = self.atoms.RECV;

        // Start from a clean slate: an earlier transfer that timed out may have
        // left data behind.
        self.conn.delete_property(window, property)?;
        self.conn
            .convert_selection(window, selection, target, property, CURRENT_TIME)?;
        self.conn.flush()?;

        let deadline = Instant::now() + REPLY_TIMEOUT;
        let matched = self.wait_for(deadline, ctx, |event| {
            matches!(
                event,
                Event::SelectionNotify(notify)
                    if notify.requestor == window
                        && notify.selection == selection
                        && notify.target == target
            )
        })?;

        let Some(Event::SelectionNotify(notify)) = matched else {
            if ctx.should_stop() {
                return Ok(None);
            }
            bail!("the selection owner did not answer within {REPLY_TIMEOUT:?}");
        };

        // ICCCM: a property of None means the owner refused the conversion.
        if notify.property == NONE {
            tracing::debug!("owner refused to convert the selection to the chosen target");
            return Ok(None);
        }

        self.read_property(ctx, limit)
    }

    fn read_property(&mut self, ctx: &MonitorContext, limit: u64) -> Result<Option<Vec<u8>>> {
        let window = self.window;
        let property = self.atoms.RECV;

        // A zero-length request returns the type and the total size without
        // transferring anything, so an over-limit selection is declined before
        // a single byte crosses the socket (§33).
        let probe = self
            .conn
            .get_property(false, window, property, AtomEnum::ANY, 0, 0)?
            .reply()?;

        if probe.type_ == NONE {
            return Ok(None);
        }

        if probe.type_ == self.atoms.INCR {
            return self.read_incrementally(ctx, limit);
        }

        let total = u64::from(probe.bytes_after);
        if total > limit {
            ctx.reject(&Rejection::TooLarge { size: total, limit });
            self.conn.delete_property(window, property)?;
            self.conn.flush()?;
            return Ok(None);
        }

        let data = self.drain_property(total)?;
        self.conn.delete_property(window, property)?;
        self.conn.flush()?;
        Ok(Some(data))
    }

    /// Read the receive property to its end, in chunks.
    ///
    /// `expected` is only a capacity hint; it comes from the peer, so it is
    /// clamped rather than trusted.
    fn drain_property(&mut self, expected: u64) -> Result<Vec<u8>> {
        const CAPACITY_HINT_CAP: u64 = 8 * 1024 * 1024;

        let window = self.window;
        let property = self.atoms.RECV;
        let mut data: Vec<u8> = Vec::with_capacity(expected.min(CAPACITY_HINT_CAP) as usize);
        let mut offset = 0u32;

        loop {
            let reply = self
                .conn
                .get_property(false, window, property, AtomEnum::ANY, offset, CHUNK_WORDS)?
                .reply()?;
            if reply.value.is_empty() {
                break;
            }
            // Every chunk but the last is a whole number of 32-bit words, which
            // is what the protocol's word-based offset requires.
            offset += (reply.value.len() / 4) as u32;
            data.extend_from_slice(&reply.value);
            if reply.bytes_after == 0 {
                break;
            }
        }
        Ok(data)
    }

    /// The `INCR` protocol: the owner streams the data in chunks, each
    /// announced by a `PropertyNotify`, each acknowledged by us deleting the
    /// property.
    fn read_incrementally(
        &mut self,
        ctx: &MonitorContext,
        limit: u64,
    ) -> Result<Option<Vec<u8>>> {
        let window = self.window;
        let property = self.atoms.RECV;
        let overall_deadline = Instant::now() + TRANSFER_TIMEOUT;

        let mut data: Vec<u8> = Vec::new();
        // Once over the limit we stop accumulating but keep acknowledging
        // chunks, so the owner's transfer completes instead of hanging.
        let mut over_limit: Option<u64> = None;

        loop {
            // Deleting the property is the acknowledgement that starts the next
            // chunk; the first delete tells the owner we are ready at all.
            self.conn.delete_property(window, property)?;
            self.conn.flush()?;

            let deadline = overall_deadline.min(Instant::now() + CHUNK_TIMEOUT);
            let matched = self.wait_for(deadline, ctx, |event| {
                matches!(
                    event,
                    Event::PropertyNotify(notify)
                        if notify.window == window
                            && notify.atom == property
                            && notify.state == Property::NEW_VALUE
                )
            })?;
            if matched.is_none() {
                if ctx.should_stop() {
                    return Ok(None);
                }
                bail!("the incremental transfer stalled");
            }

            let chunk = self.drain_property(0)?;
            // A zero-length chunk is how the owner signals the end.
            if chunk.is_empty() {
                break;
            }

            if over_limit.is_none() {
                let size = data.len() as u64 + chunk.len() as u64;
                if size > limit {
                    over_limit = Some(size);
                    data = Vec::new();
                } else {
                    data.extend_from_slice(&chunk);
                }
            }

            if Instant::now() >= overall_deadline {
                self.conn.delete_property(window, property)?;
                self.conn.flush()?;
                bail!("the incremental transfer exceeded {TRANSFER_TIMEOUT:?}");
            }
        }

        self.conn.delete_property(window, property)?;
        self.conn.flush()?;

        if let Some(size) = over_limit {
            // The size reported is a lower bound: we stopped counting once the
            // limit was passed.
            ctx.reject(&Rejection::TooLarge { size, limit });
            return Ok(None);
        }
        Ok(Some(data))
    }
}

impl ClipboardBackend for X11Backend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn run(&mut self, ctx: &MonitorContext) -> Result<()> {
        let wake = Arc::clone(&self.wake);
        ctx.set_waker(Box::new(move || wake.wake()));

        self.capture_current(ctx);

        loop {
            if ctx.should_stop() {
                return Ok(());
            }
            if let Some(event) = self.take_event()? {
                self.dispatch(event, ctx);
                continue;
            }
            self.conn.flush()?;

            match wake::wait(self.conn.stream().as_fd(), self.wake.read_fd(), None)? {
                Ready::Socket => {}
                Ready::Woken => self.wake.drain(),
                Ready::TimedOut => {}
            }
        }
    }
}

impl Drop for X11Backend {
    fn drop(&mut self) {
        // Best effort: the connection is about to close, which destroys the
        // window anyway, but being explicit keeps the server's resource
        // accounting tidy if the connection outlives us for any reason.
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
}
