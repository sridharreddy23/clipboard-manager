//! The clipboard engine: watching for selection changes and turning them into
//! history entries (§5, §6, §9, §35, §45).
//!
//! # Event-driven, not polled
//!
//! There is no timer anywhere in this module. A backend blocks on its display
//! server connection — `XFixesSelectSelectionInput` under X11, the
//! `zwlr_data_control_offer` stream under Wayland — and wakes only when a
//! selection actually changes. An idle session therefore costs zero wake-ups,
//! which is the whole point of §45.
//!
//! # Thread split
//!
//! ```text
//!   monitor thread                                main (GTK) thread
//!   ──────────────                                ─────────────────
//!   wait for selection change
//!   choose a MIME target        ── privacy ──▶ (dropped here, never read)
//!   retrieve bytes
//!   privacy check
//!   normalise, hash, write blob
//!   send MonitorEvent  ───────── async-channel ──▶  Store::insert
//! ```
//!
//! Hashing and the blob write happen on the monitor thread *before* the event
//! is posted, so copying a 50 MB screenshot never stalls the UI. The main
//! thread only ever receives a small struct plus a blob hash.
//!
//! # Supervision
//!
//! Backends are built from a factory rather than passed in as an instance, so
//! that a lost display-server connection can be retried by constructing a fresh
//! one with capped exponential backoff (§35, §44). A backend that keeps failing
//! reports [`Health::Failed`] and the daemon tells the user, rather than
//! pretending to monitor the clipboard.

pub mod normalize;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::Settings;
use crate::model::{self, NewClip, RawClip, Selection};
use crate::privacy;
use crate::storage::BlobStore;

pub use normalize::{ChosenTarget, Rejection, TargetClass};

/// Settings shared with worker threads.
///
/// Written only by the main thread when the user applies new settings, so the
/// lock is uncontended in practice; a monitor thread takes a read lock once per
/// selection change.
pub type SharedSettings = Arc<RwLock<Settings>>;

pub fn shared_settings(settings: Settings) -> SharedSettings {
    Arc::new(RwLock::new(settings))
}

/// How well a backend is working. Reported honestly: §56 forbids claiming a
/// platform feature works when it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Monitoring normally.
    Running,
    /// Monitoring, but with a documented limitation the user should know about.
    Degraded { detail: String },
    /// Not monitoring. The clipboard history will not grow.
    Failed { detail: String },
}

/// A backend's identity and current health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendStatus {
    pub backend: &'static str,
    pub health: Health,
}

/// Events posted from the monitor thread to the main thread.
#[derive(Debug)]
pub enum MonitorEvent {
    /// A clipping is ready to be stored.
    Clip(Box<NewClip>),
    /// A clipping was deliberately not stored.
    ///
    /// `notify` is `Some` only for the cases a user can act on — practically,
    /// exceeding the size limit. Everything else is routine and a notification
    /// would be noise (§34).
    Dropped { reason: String, notify: Option<String> },
    /// The backend's health changed.
    Status(BackendStatus),
}

/// A platform-specific clipboard monitor.
///
/// Implementations live in [`crate::platform`]. The protocol work is theirs;
/// every policy decision — pause, PRIMARY, target choice, privacy, size limits,
/// normalisation — belongs to [`MonitorContext`] so that it cannot drift
/// between X11 and Wayland.
pub trait ClipboardBackend: Send {
    /// Short stable name, used in logs and in `status` output.
    fn name(&self) -> &'static str;

    /// A limitation to report at startup, if any. For example, a Wayland
    /// backend cannot identify the owning application, which makes the
    /// application-exclusion list inoperative there (§17).
    fn caveat(&self) -> Option<String> {
        None
    }

    /// Watch for selection changes until [`MonitorContext::should_stop`]
    /// returns true.
    ///
    /// Returning `Ok(())` means a clean shutdown. Returning `Err` means the
    /// connection broke; the supervisor will build a new backend and retry.
    fn run(&mut self, ctx: &MonitorContext) -> Result<()>;
}

/// Builds a backend. Called once per supervision attempt.
pub type BackendFactory = Box<dyn Fn() -> Result<Box<dyn ClipboardBackend>> + Send>;

/// Interrupts a backend that is parked waiting for display-server events.
///
/// A backend registers one with [`MonitorContext::set_waker`] during startup.
/// It must be safe to call from another thread and after the backend has
/// already exited — in practice each backend uses a self-pipe or its event
/// queue's own wake primitive.
pub type Waker = Box<dyn Fn() + Send + Sync>;

/// Everything a backend needs in order to hand a selection to the pipeline.
pub struct MonitorContext {
    settings: SharedSettings,
    events: async_channel::Sender<MonitorEvent>,
    blobs: BlobStore,
    stop: Arc<AtomicBool>,
    /// Content hashes to ignore once, registered by the main thread before it
    /// puts an entry back on the clipboard itself (§52). Without this, every
    /// restore would come straight back to us as a fresh selection change.
    suppressed: Arc<Mutex<Vec<String>>>,
    /// Hash of the last clipping we accepted, so an application that re-asserts
    /// ownership of unchanged content does not cost a database transaction.
    last_hash: Mutex<Option<String>>,
    /// Set by the backend so shutdown can interrupt its blocking read.
    waker: Mutex<Option<Waker>>,
}

impl MonitorContext {
    /// Current settings snapshot.
    ///
    /// Cloned rather than held under the lock: a backend may spend milliseconds
    /// in an INCR transfer, and blocking the settings window for that long
    /// would be a poor trade for one avoided allocation.
    pub fn settings(&self) -> Settings {
        match self.settings.read() {
            Ok(guard) => guard.clone(),
            // A poisoned lock means another thread panicked while holding it.
            // Defaults are a safe, privacy-preserving fallback and are better
            // than taking the monitor down.
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// True when the supervisor has been asked to shut down.
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.events.is_closed()
    }

    /// The blob store, for backends that need it directly. Cheap to clone.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Register a way to interrupt this backend's blocking read.
    ///
    /// Backends should call this before entering their event loop. Without it,
    /// a backend parked on `poll(2)` stays parked until the process exits,
    /// which is harmless but makes shutdown untestable.
    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(waker);
        }
    }

    fn wake(&self) {
        if let Ok(slot) = self.waker.lock() {
            if let Some(waker) = slot.as_ref() {
                waker();
            }
        }
    }

    /// Cheap pre-check: is this selection worth examining at all?
    ///
    /// A backend may call this to skip the round trips needed to enumerate a
    /// selection's MIME types when the answer is already no — for example
    /// PRIMARY, which changes on every mouse drag and is off by default.
    /// [`Self::wants`] applies the same rule authoritatively, so this is an
    /// optimisation and never the only gate.
    pub fn interested(&self, selection: Selection) -> bool {
        let settings = self.settings();
        !settings.privacy.paused
            && (selection == Selection::Clipboard || settings.clipboard.monitor_primary)
    }

    /// Decide whether to retrieve this selection at all, and with which target.
    ///
    /// Called *before* the payload is read, which matters: when a password
    /// manager flags a selection as a secret, or the owning application is
    /// excluded, we never ask the display server for the bytes in the first
    /// place (§16).
    pub fn wants(
        &self,
        selection: Selection,
        offered: &[String],
        app: Option<&model::AppIdentity>,
    ) -> Option<ChosenTarget> {
        if !self.interested(selection) {
            return None;
        }
        let settings = self.settings();

        // Content heuristics need the text, so they are applied in `submit`.
        if let Some(block) = privacy::check(&settings.privacy, offered, app, None) {
            self.send(MonitorEvent::Dropped { reason: block.reason(), notify: None });
            return None;
        }

        let chosen = normalize::choose_target(offered, &settings);
        if chosen.is_none() {
            tracing::debug!(
                targets = offered.len(),
                "selection offered nothing we can store"
            );
        }
        chosen
    }

    /// Register a content hash to ignore the next time it appears.
    ///
    /// Used by the daemon when it writes an entry back to the clipboard.
    pub fn suppress(&self, content_hash: &str) {
        if let Ok(mut pending) = self.suppressed.lock() {
            // Bounded: if a suppression never matches (the user copied
            // something else first) it must not accumulate forever.
            if pending.len() >= 8 {
                pending.remove(0);
            }
            pending.push(content_hash.to_string());
        }
    }

    /// Hand a retrieved selection to the pipeline.
    ///
    /// Does the privacy content check, normalises, writes any blob, and posts
    /// the result. Never returns an error: one unprocessable clipping must not
    /// stop monitoring (§35).
    pub fn submit(&self, raw: RawClip) {
        if self.should_stop() {
            return;
        }
        let settings = self.settings();

        // Re-checked here rather than relying on `wants`, so that a backend
        // which submits without asking first still cannot bypass privacy. The
        // payload is only decoded when the content heuristic is enabled, so the
        // common path does not scan a 50 MB image looking for UTF-8.
        let text = settings
            .privacy
            .skip_secret_like_text
            .then(|| std::str::from_utf8(&raw.bytes).ok())
            .flatten();
        if let Some(block) =
            privacy::check(&settings.privacy, &raw.offered, raw.source_app.as_ref(), text)
        {
            self.send(MonitorEvent::Dropped { reason: block.reason(), notify: None });
            return;
        }

        let clip = match normalize::normalize(&raw, &settings, &self.blobs) {
            Ok(Ok(clip)) => clip,
            Ok(Err(rejection)) => {
                tracing::debug!(reason = rejection.reason(), "clipping not stored");
                self.send(MonitorEvent::Dropped {
                    reason: rejection.reason().to_string(),
                    notify: rejection.user_message(),
                });
                return;
            }
            Err(err) => {
                // Normalisation only fails on I/O, i.e. the blob write.
                tracing::error!(error = %err, "failed to process a clipping");
                self.send(MonitorEvent::Dropped {
                    reason: "could not be saved".to_string(),
                    notify: Some(
                        "Clipboard item not saved: storage is unavailable.".to_string(),
                    ),
                });
                return;
            }
        };

        if self.take_suppression(&clip.content_hash) {
            tracing::debug!(
                hash = model::short_hash(&clip.content_hash),
                "ignoring our own clipboard write"
            );
            return;
        }
        if self.is_repeat(&clip.content_hash) {
            tracing::debug!(
                hash = model::short_hash(&clip.content_hash),
                "ownership re-asserted with unchanged content"
            );
            return;
        }

        tracing::debug!(
            kind = %clip.kind,
            size = clip.size_bytes,
            hash = model::short_hash(&clip.content_hash),
            "captured a clipping"
        );
        self.send(MonitorEvent::Clip(Box::new(clip)));
    }

    /// Report that a selection was deliberately not retrieved.
    ///
    /// Lets a backend decline a transfer it can tell in advance is pointless —
    /// in practice one that the display server has already told us is larger
    /// than the configured limit — while keeping the reason and the decision
    /// about whether to notify in one place (§33, §34).
    pub fn reject(&self, rejection: &Rejection) {
        tracing::debug!(reason = rejection.reason(), "clipping not retrieved");
        self.send(MonitorEvent::Dropped {
            reason: rejection.reason().to_string(),
            notify: rejection.user_message(),
        });
    }

    /// Report a health change.
    pub fn report(&self, backend: &'static str, health: Health) {
        self.send(MonitorEvent::Status(BackendStatus { backend, health }));
    }

    fn take_suppression(&self, hash: &str) -> bool {
        let Ok(mut pending) = self.suppressed.lock() else {
            return false;
        };
        if let Some(idx) = pending.iter().position(|h| h == hash) {
            pending.remove(idx);
            // Treat a suppressed write as the current clipboard content, so the
            // dedupe below also covers a backend that reports it twice.
            if let Ok(mut last) = self.last_hash.lock() {
                *last = Some(hash.to_string());
            }
            return true;
        }
        false
    }

    fn is_repeat(&self, hash: &str) -> bool {
        let Ok(mut last) = self.last_hash.lock() else {
            return false;
        };
        if last.as_deref() == Some(hash) {
            return true;
        }
        *last = Some(hash.to_string());
        false
    }

    fn send(&self, event: MonitorEvent) {
        // The channel is unbounded, so this cannot block the monitor thread.
        // A closed channel means the main thread has gone; the supervisor's
        // next `should_stop` will notice.
        let _ = self.events.send_blocking(event);
    }
}

/// Handle to the running monitor.
pub struct Monitor {
    events: async_channel::Receiver<MonitorEvent>,
    ctx: Arc<MonitorContext>,
    stop: Arc<AtomicBool>,
    name: &'static str,
}

/// Backoff schedule for reconnecting after a backend failure, in seconds.
const RETRY_BACKOFF_SECS: [u64; 7] = [1, 2, 4, 8, 15, 30, 30];

/// A run lasting at least this long is treated as a success, resetting the
/// backoff. Without this, a backend that works for hours and then drops once
/// would still be counted towards the give-up limit.
const RETRY_RESET_AFTER: Duration = Duration::from_secs(60);

/// Consecutive failed attempts before we stop retrying and say so.
const MAX_CONSECUTIVE_FAILURES: usize = 12;

impl Monitor {
    /// Start monitoring on a dedicated thread.
    ///
    /// `factory` is called once per attempt, so a backend may hold a display
    /// server connection without having to make it reconnectable itself.
    pub fn spawn(
        name: &'static str,
        factory: BackendFactory,
        settings: SharedSettings,
        blobs: BlobStore,
    ) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Arc::new(MonitorContext {
            settings,
            events: tx,
            blobs,
            stop: Arc::clone(&stop),
            suppressed: Arc::new(Mutex::new(Vec::new())),
            last_hash: Mutex::new(None),
            waker: Mutex::new(None),
        });

        let thread_ctx = Arc::clone(&ctx);
        let builder = std::thread::Builder::new().name("clip-monitor".to_string());
        // A failure to spawn a thread is not recoverable by retrying, so it is
        // reported as a failed backend rather than propagated.
        if let Err(err) = builder.spawn(move || supervise(name, factory, thread_ctx)) {
            tracing::error!(error = %err, "could not start the clipboard monitor thread");
            ctx.report(
                name,
                Health::Failed { detail: format!("could not start monitor thread: {err}") },
            );
        }

        Self { events: rx, ctx, stop, name }
    }

    /// The event stream, for `glib::spawn_future_local` on the main thread.
    pub fn events(&self) -> async_channel::Receiver<MonitorEvent> {
        self.events.clone()
    }

    pub fn backend_name(&self) -> &'static str {
        self.name
    }

    /// Ignore the next appearance of this content, because we are about to put
    /// it on the clipboard ourselves.
    pub fn suppress(&self, content_hash: &str) {
        self.ctx.suppress(content_hash);
    }

    /// A detached handle for suppressing, usable without borrowing the monitor.
    ///
    /// The UI needs to suppress a hash from inside a widget callback, where
    /// holding a reference to the `Monitor` would tangle the ownership graph —
    /// the daemon owns both, and the popup would have to borrow back up through
    /// it. The returned closure holds only the shared context.
    pub fn suppressor(&self) -> impl Fn(&str) + Clone + 'static {
        let ctx = Arc::clone(&self.ctx);
        move |content_hash: &str| ctx.suppress(content_hash)
    }

    /// Ask the monitor to stop.
    ///
    /// Sets the stop flag, closes the event channel and interrupts the
    /// backend's blocking read via the waker it registered. Deliberately does
    /// **not** join: the thread holds no lock the shutdown path needs, and a
    /// backend without a waker would otherwise stall shutdown until its next
    /// display-server event.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.events.close();
        self.ctx.wake();
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Build, run and if necessary rebuild a backend until asked to stop.
fn supervise(name: &'static str, factory: BackendFactory, ctx: Arc<MonitorContext>) {
    let mut failures = 0usize;

    while !ctx.should_stop() {
        let started = Instant::now();

        let outcome = match factory() {
            Ok(mut backend) => {
                let caveat = backend.caveat();
                ctx.report(
                    name,
                    match caveat {
                        Some(detail) => Health::Degraded { detail },
                        None => Health::Running,
                    },
                );
                backend.run(&ctx)
            }
            Err(err) => Err(err),
        };

        if ctx.should_stop() {
            return;
        }

        match outcome {
            Ok(()) => {
                tracing::info!(backend = name, "clipboard monitor stopped");
                return;
            }
            Err(err) => {
                if started.elapsed() >= RETRY_RESET_AFTER {
                    failures = 0;
                }
                failures += 1;
                tracing::warn!(
                    backend = name,
                    attempt = failures,
                    error = %err,
                    "clipboard monitor failed; will retry"
                );

                if failures >= MAX_CONSECUTIVE_FAILURES {
                    tracing::error!(
                        backend = name,
                        "giving up on the clipboard monitor after {failures} attempts"
                    );
                    ctx.report(
                        name,
                        Health::Failed {
                            detail: format!(
                                "stopped after {failures} failed attempts: {err}"
                            ),
                        },
                    );
                    return;
                }

                let wait = RETRY_BACKOFF_SECS
                    [(failures - 1).min(RETRY_BACKOFF_SECS.len() - 1)];
                ctx.report(
                    name,
                    Health::Degraded {
                        detail: format!("reconnecting in {wait}s after an error: {err}"),
                    },
                );
                // Sleep in slices so shutdown is not delayed by the backoff.
                let deadline = Instant::now() + Duration::from_secs(wait);
                while Instant::now() < deadline && !ctx.should_stop() {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AppIdentity;

    struct Harness {
        _tmp: tempfile::TempDir,
        ctx: Arc<MonitorContext>,
        events: async_channel::Receiver<MonitorEvent>,
        stop: Arc<AtomicBool>,
    }

    fn harness(settings: Settings) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(tmp.path().join("blobs")).unwrap();
        let (tx, rx) = async_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Arc::new(MonitorContext {
            settings: shared_settings(settings),
            events: tx,
            blobs,
            stop: Arc::clone(&stop),
            suppressed: Arc::new(Mutex::new(Vec::new())),
            last_hash: Mutex::new(None),
            waker: Mutex::new(None),
        });
        Harness { _tmp: tmp, ctx, events: rx, stop }
    }

    impl Harness {
        fn drain(&self) -> Vec<MonitorEvent> {
            let mut out = Vec::new();
            while let Ok(ev) = self.events.try_recv() {
                out.push(ev);
            }
            out
        }

        fn clips(&self) -> Vec<NewClip> {
            self.drain()
                .into_iter()
                .filter_map(|e| match e {
                    MonitorEvent::Clip(c) => Some(*c),
                    _ => None,
                })
                .collect()
        }
    }

    fn raw(mime: &str, bytes: &[u8]) -> RawClip {
        RawClip {
            offered: vec![mime.to_string()],
            mime: mime.to_string(),
            bytes: bytes.to_vec(),
            selection: Selection::Clipboard,
            source_app: None,
        }
    }

    #[test]
    fn a_text_selection_becomes_a_clip_event() {
        let h = harness(Settings::default());
        h.ctx.submit(raw("text/plain", b"hello"));
        let clips = h.clips();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].text_content.as_deref(), Some("hello"));
    }

    #[test]
    fn pausing_stops_us_before_the_payload_is_ever_read() {
        let mut s = Settings::default();
        s.privacy.paused = true;
        let h = harness(s);
        assert!(h.ctx.wants(Selection::Clipboard, &["text/plain".into()], None).is_none());
    }

    #[test]
    fn primary_is_ignored_unless_enabled() {
        let h = harness(Settings::default());
        let offered = vec!["text/plain".to_string()];
        assert!(h.ctx.wants(Selection::Primary, &offered, None).is_none());
        assert!(h.ctx.wants(Selection::Clipboard, &offered, None).is_some());

        let mut s = Settings::default();
        s.clipboard.monitor_primary = true;
        let h = harness(s);
        assert!(h.ctx.wants(Selection::Primary, &offered, None).is_some());
    }

    #[test]
    fn a_password_hint_prevents_retrieval_entirely() {
        let h = harness(Settings::default());
        let offered = vec!["text/plain".to_string(), "x-kde-passwordManagerHint".to_string()];
        assert!(h.ctx.wants(Selection::Clipboard, &offered, None).is_none());

        // And the user is told nothing, because this is routine (§34).
        let events = h.drain();
        assert!(matches!(
            events.as_slice(),
            [MonitorEvent::Dropped { notify: None, .. }]
        ));
    }

    #[test]
    fn an_excluded_application_prevents_retrieval_entirely() {
        let h = harness(Settings::default());
        let app = AppIdentity { name: Some("keepassxc".into()), ..Default::default() };
        assert!(h
            .ctx
            .wants(Selection::Clipboard, &["text/plain".into()], Some(&app))
            .is_none());
        assert_eq!(h.drain().len(), 1, "exactly one Dropped event");
    }

    #[test]
    fn an_oversized_clipping_notifies_the_user_and_stores_nothing() {
        let mut s = Settings::default();
        s.clipboard.max_item_size_mb = 1;
        let h = harness(s);
        h.ctx.submit(raw("text/plain", &vec![b'x'; 2 * 1024 * 1024]));

        let events = h.drain();
        match events.as_slice() {
            [MonitorEvent::Dropped { notify: Some(msg), .. }] => {
                assert!(msg.contains("exceeds"), "{msg}");
            }
            other => panic!("expected one notifying Dropped, got {other:?}"),
        }
    }

    #[test]
    fn re_asserted_ownership_of_unchanged_content_is_not_resubmitted() {
        let h = harness(Settings::default());
        h.ctx.submit(raw("text/plain", b"same"));
        h.ctx.submit(raw("text/plain", b"same"));
        h.ctx.submit(raw("text/plain", b"same"));
        assert_eq!(h.clips().len(), 1);

        // Different content gets through, and then the first can repeat.
        h.ctx.submit(raw("text/plain", b"other"));
        h.ctx.submit(raw("text/plain", b"same"));
        assert_eq!(h.clips().len(), 2);
    }

    #[test]
    fn our_own_clipboard_write_is_suppressed_once() {
        let h = harness(Settings::default());

        // Learn the hash the pipeline will produce for this content.
        h.ctx.submit(raw("text/plain", b"restored"));
        let hash = h.clips().pop().unwrap().content_hash;

        // A different clipping, so the repeat filter is not what is being
        // tested here.
        h.ctx.submit(raw("text/plain", b"interleaved"));
        assert_eq!(h.clips().len(), 1);

        h.ctx.suppress(&hash);
        h.ctx.submit(raw("text/plain", b"restored"));
        assert!(h.clips().is_empty(), "the restore echoed back into the history");

        // The suppression is consumed, so a genuine later copy is recorded.
        h.ctx.submit(raw("text/plain", b"interleaved"));
        h.ctx.submit(raw("text/plain", b"restored"));
        assert_eq!(h.clips().len(), 2);
    }

    #[test]
    fn pending_suppressions_cannot_grow_without_bound() {
        let h = harness(Settings::default());
        for i in 0..100 {
            h.ctx.suppress(&format!("hash-{i}"));
        }
        assert_eq!(h.ctx.suppressed.lock().unwrap().len(), 8);
    }

    #[test]
    fn stopping_makes_submit_a_no_op() {
        let h = harness(Settings::default());
        h.stop.store(true, Ordering::Relaxed);
        h.ctx.submit(raw("text/plain", b"too late"));
        assert!(h.clips().is_empty());
        assert!(h.ctx.should_stop());
    }

    #[test]
    fn a_closed_channel_is_treated_as_shutdown() {
        let h = harness(Settings::default());
        h.events.close();
        assert!(h.ctx.should_stop());
    }

    // -- supervision ----------------------------------------------------------

    /// A backend that fails `fail_times` times and then runs until stopped.
    struct FlakyBackend {
        attempts: Arc<Mutex<usize>>,
    }

    impl ClipboardBackend for FlakyBackend {
        fn name(&self) -> &'static str {
            "flaky"
        }
        fn run(&mut self, ctx: &MonitorContext) -> Result<()> {
            let attempt = {
                let mut a = self.attempts.lock().unwrap();
                *a += 1;
                *a
            };
            if attempt <= 2 {
                anyhow::bail!("connection lost");
            }
            ctx.submit(raw("text/plain", b"recovered"));
            while !ctx.should_stop() {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        }
    }

    #[test]
    fn a_backend_that_drops_its_connection_is_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(tmp.path().join("blobs")).unwrap();
        let attempts = Arc::new(Mutex::new(0usize));

        let factory_attempts = Arc::clone(&attempts);
        let monitor = Monitor::spawn(
            "flaky",
            Box::new(move || {
                Ok(Box::new(FlakyBackend { attempts: Arc::clone(&factory_attempts) })
                    as Box<dyn ClipboardBackend>)
            }),
            shared_settings(Settings::default()),
            blobs,
        );

        // Two failures take 1s + 2s of backoff, so allow generous headroom.
        let events = monitor.events();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut recovered = false;
        let mut degraded = 0usize;
        while Instant::now() < deadline && !recovered {
            match events.recv_blocking() {
                Ok(MonitorEvent::Clip(clip)) => {
                    assert_eq!(clip.text_content.as_deref(), Some("recovered"));
                    recovered = true;
                }
                Ok(MonitorEvent::Status(BackendStatus {
                    health: Health::Degraded { .. },
                    ..
                })) => degraded += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }

        assert!(recovered, "the supervisor never rebuilt the backend");
        assert!(degraded >= 2, "retries should be reported as degraded health");
        assert_eq!(*attempts.lock().unwrap(), 3);
        monitor.stop();
    }

    #[test]
    fn a_backend_that_cannot_be_built_at_all_reports_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(tmp.path().join("blobs")).unwrap();

        let monitor = Monitor::spawn(
            "broken",
            Box::new(|| anyhow::bail!("no display server")),
            shared_settings(Settings::default()),
            blobs,
        );

        // The give-up path is reached only after MAX_CONSECUTIVE_FAILURES, so
        // assert on the first report instead of waiting out the backoff.
        let events = monitor.events();
        let first = events.recv_blocking().unwrap();
        match first {
            MonitorEvent::Status(BackendStatus { backend, health: Health::Degraded { detail } }) => {
                assert_eq!(backend, "broken");
                assert!(detail.contains("no display server"), "{detail}");
            }
            other => panic!("expected a degraded status, got {other:?}"),
        }
        monitor.stop();
    }

    #[test]
    fn a_caveat_is_reported_as_degraded_rather_than_hidden() {
        struct Caveated;
        impl ClipboardBackend for Caveated {
            fn name(&self) -> &'static str {
                "caveated"
            }
            fn caveat(&self) -> Option<String> {
                Some("source application cannot be detected".to_string())
            }
            fn run(&mut self, ctx: &MonitorContext) -> Result<()> {
                while !ctx.should_stop() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(tmp.path().join("blobs")).unwrap();
        let monitor = Monitor::spawn(
            "caveated",
            Box::new(|| Ok(Box::new(Caveated) as Box<dyn ClipboardBackend>)),
            shared_settings(Settings::default()),
            blobs,
        );

        match monitor.events().recv_blocking().unwrap() {
            MonitorEvent::Status(BackendStatus { health: Health::Degraded { detail }, .. }) => {
                assert!(detail.contains("cannot be detected"));
            }
            other => panic!("expected degraded, got {other:?}"),
        }
        monitor.stop();
    }

    #[test]
    fn settings_changes_are_picked_up_without_restarting_the_monitor() {
        let settings = shared_settings(Settings::default());
        let tmp = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(tmp.path().join("blobs")).unwrap();
        let (tx, _rx) = async_channel::unbounded();
        let ctx = MonitorContext {
            settings: Arc::clone(&settings),
            events: tx,
            blobs,
            stop: Arc::new(AtomicBool::new(false)),
            suppressed: Arc::new(Mutex::new(Vec::new())),
            last_hash: Mutex::new(None),
            waker: Mutex::new(None),
        };

        let offered = vec!["image/png".to_string()];
        assert_eq!(
            ctx.wants(Selection::Clipboard, &offered, None).map(|c| c.class),
            Some(TargetClass::Image)
        );

        settings.write().unwrap().clipboard.store_images = false;
        assert!(ctx.wants(Selection::Clipboard, &offered, None).is_none());
    }
}
