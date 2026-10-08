//! End-to-end checks against a real display server (§43).
//!
//! These are the tests that prove the platform backends *work*, rather than
//! merely compile. They are also the only tests in this project that reach
//! outside it, and that is why **every one of them is `#[ignore]`d**:
//!
//! ```text
//! cargo test --test platform_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! # Why they are opt-in
//!
//! They act on the *live session*, not on a fixture:
//!
//! * They take ownership of the user's real `CLIPBOARD` selection, so running
//!   them replaces whatever the user had copied, and a failure part-way can
//!   leave a test payload sitting there.
//! * They create windows on, and hold a connection to, the real display
//!   server. Under a GNOME Wayland session that is the XWayland server the
//!   compositor started — a separate process, so nothing here can crash the
//!   desktop, but it is still shared state belonging to the user's session
//!   rather than to the test.
//!
//! Neither is acceptable as a side effect of a plain `cargo test`, which a
//! developer runs dozens of times an hour. Opting in is a deliberate act, and
//! `--test-threads=1` is part of the documented invocation because there is
//! only one system clipboard to go round (see [`CLIPBOARD`]).
//!
//! Each test still checks for a usable display first and reports a skip rather
//! than failing, so a headless CI runner that *does* pass `--ignored` stays
//! green.
//!
//! The selection owner here is a real ICCCM owner built on a second X11
//! connection rather than a call to `xclip`, for two reasons: the suite must
//! not depend on tools that may not be installed, and owning the selection
//! ourselves is what exercises the `TARGETS` negotiation and the property
//! transfer that the backend is actually made of.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clipboard_manager::clipboard::{shared_settings, Monitor, MonitorEvent};
use clipboard_manager::config::Settings;
use clipboard_manager::model::{ClipKind, NewClip};
use clipboard_manager::platform::{self, DisplayServer, Session};
use clipboard_manager::storage::BlobStore;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

/// How long to wait for a clipping to make it through the pipeline. Generous:
/// a loaded CI machine is slow, and a flaky test is worse than a slow one.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

// -- harness ------------------------------------------------------------------

/// Is there an X11 display we can talk to?
///
/// Checked by connecting, not by looking at `DISPLAY`: the variable can be set
/// in an environment where nothing is listening.
fn x11_available() -> bool {
    match x11rb::connect(None) {
        Ok(_) => true,
        Err(err) => {
            eprintln!("skipping: no usable X11 display ({err})");
            false
        }
    }
}

/// Serialises the tests that touch the clipboard.
///
/// There is exactly one system clipboard, and `cargo test` runs test functions
/// concurrently by default, so without this two tests race: one sets the
/// selection while the other is waiting to capture its own, and whichever
/// assertion loses sees the other test's payload. Holding this for the whole
/// body of each clipboard test — rather than reaching for `--test-threads=1`,
/// which callers would have to remember — makes them correct under the default
/// invocation.
static CLIPBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Claim the clipboard for the rest of the current test.
///
/// Must be the first thing a clipboard test does: one of them sets the
/// selection *before* starting its monitor, so taking the lock any later would
/// leave that window unguarded.
fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
    // A panicking test poisons the lock; the clipboard is still usable, so
    // recovering is right and failing the *next* test for it would be actively
    // misleading.
    CLIPBOARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Harness {
    monitor: Monitor,
    events: async_channel::Receiver<MonitorEvent>,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("could not create a temporary directory");
        let blobs = BlobStore::open(dir.path().join("blobs")).expect("could not open blob store");

        let mut settings = Settings::default();
        // The harness copies deliberately, so nothing should be filtered out
        // from under the assertions.
        settings.privacy.paused = false;
        settings.clipboard.monitor_primary = false;

        let monitor = Monitor::spawn(
            platform::x11::BACKEND_NAME,
            Box::new(platform::x11::build),
            shared_settings(settings),
            blobs,
        );
        let events = monitor.events();
        Self { monitor, events, _dir: dir }
    }

    /// Wait for the next captured clipping, ignoring status reports.
    fn next_clip(&self) -> Option<Box<NewClip>> {
        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        while Instant::now() < deadline {
            match self.events.try_recv() {
                Ok(MonitorEvent::Clip(clip)) => return Some(clip),
                Ok(MonitorEvent::Status(status)) => {
                    eprintln!("backend status: {:?}", status.health);
                }
                Ok(MonitorEvent::Dropped { reason, .. }) => {
                    eprintln!("clipping dropped: {reason}");
                }
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => return None,
            }
        }
        None
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.monitor.stop();
    }
}

// -- a real selection owner ---------------------------------------------------

/// An ICCCM-conformant `CLIPBOARD` owner running on its own connection and
/// thread, serving one text payload.
///
/// This is the counterpart the backend is designed against: it advertises
/// `TARGETS`, answers `UTF8_STRING`, and refuses everything else.
struct Owner {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Owner {
    /// Take ownership of `CLIPBOARD` and serve `payload` until dropped.
    ///
    /// Returns once ownership is established, so a caller can rely on the
    /// selection being set when this function returns.
    fn claim(payload: &str) -> Owner {
        let payload = payload.to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            if let Err(err) = serve(&payload, &thread_stop, ready_tx) {
                eprintln!("test selection owner failed: {err:#}");
            }
        });

        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the test selection owner did not take ownership");

        Owner { stop, thread: Some(thread) }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    payload: &str,
    stop: &AtomicBool,
    ready: std::sync::mpsc::Sender<()>,
) -> anyhow::Result<()> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;

    let clipboard = intern(&conn, b"CLIPBOARD")?;
    let targets = intern(&conn, b"TARGETS")?;
    let utf8 = intern(&conn, b"UTF8_STRING")?;

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
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    conn.set_selection_owner(window, clipboard, CURRENT_TIME)?;
    conn.flush()?;

    // Confirm the server agrees before telling the test we are ready.
    let owner = conn.get_selection_owner(clipboard)?.reply()?.owner;
    anyhow::ensure!(owner == window, "the X server gave CLIPBOARD to someone else");
    let _ = ready.send(());

    while !stop.load(Ordering::Relaxed) {
        let Some(event) = conn.poll_for_event()? else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };

        let Event::SelectionRequest(request) = event else {
            continue;
        };

        let granted = if request.target == targets {
            let atoms = [targets, utf8];
            let bytes: Vec<u8> = atoms.iter().flat_map(|a| a.to_ne_bytes()).collect();
            conn.change_property(
                PropMode::REPLACE,
                request.requestor,
                request.property,
                Atom::from(AtomEnum::ATOM),
                32,
                atoms.len() as u32,
                &bytes,
            )?;
            true
        } else if request.target == utf8 {
            conn.change_property(
                PropMode::REPLACE,
                request.requestor,
                request.property,
                utf8,
                8,
                payload.len() as u32,
                payload.as_bytes(),
            )?;
            true
        } else {
            false
        };

        // ICCCM: a property of None tells the requestor we refused.
        conn.send_event(
            false,
            request.requestor,
            EventMask::NO_EVENT,
            SelectionNotifyEvent {
                response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: request.time,
                requestor: request.requestor,
                selection: request.selection,
                target: request.target,
                property: if granted { request.property } else { NONE },
            },
        )?;
        conn.flush()?;
    }

    cleanup(&conn, window, clipboard);
    Ok(())
}

fn intern(conn: &RustConnection, name: &[u8]) -> anyhow::Result<Atom> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn cleanup(conn: &RustConnection, window: Window, clipboard: Atom) {
    // Releasing ownership explicitly keeps the user's real clipboard from being
    // left pointing at a dead test window.
    let _ = conn.set_selection_owner(NONE, clipboard, CURRENT_TIME);
    let _ = conn.destroy_window(window);
    let _ = conn.flush();
}

// -- tests --------------------------------------------------------------------

#[test]
#[ignore = "acts on the live session: see this file's module docs"]
fn an_x11_selection_set_before_startup_is_captured() {
    if !x11_available() {
        return;
    }
    let _clipboard = clipboard_lock();

    // Ownership first, monitor second: this is the restart case, where the
    // entry the user copied a moment before the daemon started must still be
    // picked up by the startup scan.
    let _owner = Owner::claim("ffmpeg -i input.mp4 output.ts");
    let harness = Harness::start();

    let clip = harness
        .next_clip()
        .expect("the backend did not capture the selection that was already set");

    assert_eq!(clip.kind, ClipKind::Text);
    assert_eq!(clip.text_content.as_deref(), Some("ffmpeg -i input.mp4 output.ts"));
}

#[test]
#[ignore = "acts on the live session: see this file's module docs"]
fn an_x11_selection_change_is_captured_through_xfixes() {
    if !x11_available() {
        return;
    }
    let _clipboard = clipboard_lock();

    // Monitor first, ownership second: this is the steady-state case, and it
    // only passes if XFIXES notifications are really arriving.
    let harness = Harness::start();
    // Let the backend finish its startup scan so the clipping below cannot be
    // confused with whatever happened to be on the clipboard already.
    std::thread::sleep(Duration::from_millis(250));
    while harness.events.try_recv().is_ok() {}

    let _owner = Owner::claim("sudo systemctl restart nginx");

    let clip = harness
        .next_clip()
        .expect("the backend did not report the selection change");

    assert_eq!(clip.kind, ClipKind::Text);
    assert_eq!(clip.text_content.as_deref(), Some("sudo systemctl restart nginx"));
    // Multi-line and Unicode fidelity is covered by the unit tests; what this
    // asserts is that the bytes survived the round trip through the protocol.
    assert_eq!(clip.size_bytes, "sudo systemctl restart nginx".len() as u64);
}

/// The claim this exercises is the load-bearing one for GNOME: that a selection
/// set by a **native Wayland** client is visible to an XWayland X11 client,
/// because mutter mirrors the two clipboards. The whole GNOME Wayland strategy
/// rests on it, so it is checked rather than asserted in prose.
///
/// `wl-copy` is used as a stand-in for an arbitrary native Wayland client. The
/// application itself never shells out to it — see `platform::mod` — but a test
/// needs *some* way to produce a genuine `wl_data_source`, and this is the one
/// that does not require a focused window. The test skips when it is absent.
#[test]
#[ignore = "acts on the live session: see this file's module docs"]
fn a_native_wayland_selection_reaches_the_xwayland_backend() {
    let session = Session::detect();
    if session.server != DisplayServer::Wayland {
        eprintln!("skipping: not a Wayland session");
        return;
    }
    if !session.has_xwayland() || !x11_available() {
        eprintln!("skipping: no XWayland display to read through");
        return;
    }
    if which("wl-copy").is_none() {
        eprintln!("skipping: wl-copy is not installed, so no native Wayland client is available");
        return;
    }
    let _clipboard = clipboard_lock();

    let harness = Harness::start();
    std::thread::sleep(Duration::from_millis(250));
    while harness.events.try_recv().is_ok() {}

    let payload = "native wayland selection mirrored into X11";
    let copied = std::process::Command::new("wl-copy")
        .arg("--")
        .arg(payload)
        .status();
    match copied {
        Ok(status) if status.success() => {}
        other => {
            eprintln!("skipping: wl-copy could not set the selection ({other:?})");
            return;
        }
    }

    let clip = harness.next_clip();
    // Clear up before asserting, so a failure does not leave the test payload
    // sitting on the user's clipboard.
    let _ = std::process::Command::new("wl-copy").arg("--clear").status();

    let clip = clip.expect(
        "a native Wayland selection was not visible through XWayland; the GNOME \
         Wayland fallback documented in platform::mod does not hold on this compositor",
    );
    assert_eq!(clip.text_content.as_deref(), Some(payload));
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[test]
#[ignore = "acts on the live session: see this file's module docs"]
fn the_backend_chosen_for_this_session_matches_what_is_actually_available() {
    let session = Session::detect();
    eprintln!("session: {}", session.describe());

    match platform::choose_backend(&session) {
        Ok(chosen) => {
            eprintln!("backend: {}", chosen.name);
            if let Some(caveat) = &chosen.caveat {
                eprintln!("caveat: {caveat}");
            }

            // A Wayland session that got the Wayland backend must genuinely
            // have the protocol — §56 forbids claiming otherwise, and this is
            // the assertion that keeps `choose_backend` honest on real hardware.
            if chosen.name == platform::wayland::BACKEND_NAME {
                assert_eq!(session.server, DisplayServer::Wayland);
                assert!(
                    platform::wayland::probe().unwrap_or(false),
                    "the Wayland backend was chosen without the protocol being present"
                );
            }

            // The XWayland fallback must come with the honest caveat attached.
            if session.server == DisplayServer::Wayland
                && chosen.name == platform::x11::BACKEND_NAME
            {
                let caveat = chosen.caveat.expect("the XWayland fallback must state its limits");
                assert!(caveat.contains("XWayland"), "{caveat}");
            }
        }
        Err(err) => {
            // Headless is a legitimate outcome, and an explicit error is the
            // documented behaviour rather than a silent no-op.
            eprintln!("skipping: no monitorable session ({err})");
            assert!(session.wayland_display.is_none() || session.x11_display.is_none());
        }
    }
}
