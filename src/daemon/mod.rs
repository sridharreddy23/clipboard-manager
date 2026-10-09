//! The long-running background service (§22, §23, §35, §49).
//!
//! One process per user session. It owns the GTK main loop and, on that thread,
//! the [`Store`]; everything that could block — reading a selection, hashing,
//! writing a blob — happens on the monitor thread and arrives as a
//! [`MonitorEvent`].
//!
//! ```text
//!   monitor thread          GTK main thread              IPC thread
//!  ┌───────────────┐      ┌──────────────────────┐     ┌──────────────┐
//!  │ X11 / Wayland │─────▶│ Core                 │◀────│ UnixListener │
//!  │ backend       │ clip │  Store (sole owner)  │ req └──────────────┘
//!  └───────────────┘      │  Popup, Settings     │
//!                         │  Tray commands       │◀────┐
//!   hotkey thread         └──────────────────────┘     │
//!  ┌───────────────┐  idle_add       │                 │
//!  │ XGrabKey      │─────────────────┘        ksni thread
//!  └───────────────┘
//! ```
//!
//! Each arrow is a channel drained by [`glib::spawn_future_local`] on the main
//! loop, so there is exactly one thread that ever touches the database or a
//! widget. That is what lets the `Store` sit behind a plain `RefCell` with no
//! lock and no chance of a write racing the UI.
//!
//! # Never root
//!
//! §28: this must run as the ordinary user. [`Core::start`] refuses to start as
//! uid 0 rather than trusting the packaging to get it right, because a daemon
//! running as root would write a root-owned history into the user's home and
//! read other users' clipboards.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{bail, Context, Result};

use crate::clipboard::{shared_settings, Health, Monitor, MonitorEvent, SharedSettings};
use crate::config::Settings;
use crate::ipc::{self, ItemSummary, Pending, Request, Response, StatusReport};
use crate::model::NewClip;
use crate::platform::{self, Session};
use crate::shortcuts::{self, Registration};
use crate::storage::Store;
use crate::ui::popup::{self, Popup};
use crate::ui::settings_window::{self, SettingsWindow};
use crate::ui::tray::{self, TrayCommand, TrayHandle};
use crate::{notify, paths};

/// How often to run database maintenance (§48).
///
/// Hourly rather than on every insert: expiry, blob collection and vacuum are
/// all cheap in aggregate but involve writes, and doing them per clipping would
/// turn a copy into a transaction storm.
const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Deferred start for the first maintenance pass, so launching stays fast
/// (§46). Long enough to be well clear of session login.
const MAINTENANCE_FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(90);

pub struct Core {
    store: Rc<RefCell<Store>>,
    settings: SharedSettings,
    popup: Rc<Popup>,
    settings_window: RefCell<Option<Rc<SettingsWindow>>>,
    monitor: Monitor,
    session: Session,
    /// Held for as long as the shortcut should stay registered.
    registration: RefCell<Registration>,
    tray: RefCell<Option<TrayHandle>>,
    /// Backend health, for `status`.
    health: RefCell<String>,
    caveat: Option<String>,
    _ipc: ipc::Server,
    _lock: ipc::InstanceLock,
}

impl Core {
    /// Start everything, in the order that makes failures recoverable.
    pub fn start() -> Result<Rc<Self>> {
        refuse_to_run_as_root()?;

        // Before any file is created: 0077 means every directory we make is
        // 0700 and every file 0600 (§28, §29).
        paths::harden_umask();

        let lock = ipc::InstanceLock::acquire()?;
        let settings = Settings::load().normalized();

        let store = Store::open(&settings)
            .context("could not open the clipboard history database")?;
        let data_dir = store.data_dir().to_path_buf();
        let store = Rc::new(RefCell::new(store));

        let shared = shared_settings(settings.clone());
        let session = Session::detect();
        tracing::info!(session = %session.describe(), "starting");

        let chosen = platform::choose_backend(&session)?;
        let caveat = chosen.caveat.clone();
        let monitor = Monitor::spawn(
            chosen.name,
            chosen.factory,
            shared.clone(),
            store.borrow().blobs().clone(),
        );

        let ipc_server = ipc::Server::start()?;

        // The popup is built before the shortcut, because the shortcut's
        // callback needs something to toggle.
        let popup = Popup::new(popup::Deps {
            store: Rc::clone(&store),
            settings: shared.clone(),
            suppress: {
                let monitor_suppress = monitor.suppressor();
                Rc::new(move |hash: &str| monitor_suppress(hash))
            },
            caveat: caveat.clone(),
            session: session.describe(),
            backend: chosen.name.to_string(),
        });

        let core = Rc::new(Self {
            store,
            settings: shared,
            popup,
            settings_window: RefCell::new(None),
            monitor,
            session,
            registration: RefCell::new(Registration::Unavailable {
                reason: "not yet registered".to_string(),
                instructions: String::new(),
                accelerator: crate::shortcuts::keys::Accelerator {
                    mods: Default::default(),
                    key: "grave".to_string(),
                },
            }),
            tray: RefCell::new(None),
            health: RefCell::new("starting".to_string()),
            caveat,
            _ipc: ipc_server,
            _lock: lock,
        });

        tracing::info!(storage = %data_dir.display(), "history ready");

        // Wire the popup's settings button now that `Core` exists. It cannot be
        // passed into `Popup::new`, because the popup has to be built first for
        // the shortcut callback to have something to toggle.
        core.popup.set_on_open_settings(Rc::new({
            let this = Rc::clone(&core);
            move || this.open_settings()
        }));

        core.register_shortcut(&settings.general.shortcut);
        core.start_tray(settings.privacy.paused && settings.general.show_tray_icon);
        core.pump_monitor();
        core.pump_ipc();
        core.schedule_maintenance();

        Ok(core)
    }

    /// Register (or re-register) the global shortcut.
    pub fn register_shortcut(self: &Rc<Self>, spec: &str) {
        // Drop the previous grab before taking a new one, or the old key stays
        // captured.
        *self.registration.borrow_mut() = Registration::Unavailable {
            reason: "re-registering".to_string(),
            instructions: String::new(),
            accelerator: crate::shortcuts::keys::Accelerator {
                mods: Default::default(),
                key: "grave".to_string(),
            },
        };
        shortcuts::unregister(&self.session);

        // The X11 grab lives on its own thread, so its callback must be
        // `Send + Sync` and cannot capture an `Rc`. It posts down a channel
        // that the main loop drains, which also means the press path is
        // identical whether it came from the grab or from `toggle` over IPC.
        let (presses_tx, presses_rx) = async_channel::bounded::<()>(1);
        let registration = shortcuts::register(&self.session, spec, move || {
            // `try_send` on a bounded(1) channel coalesces a key held down
            // into a single toggle rather than queueing a burst of them.
            let _ = presses_tx.try_send(());
        });

        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            while presses_rx.recv().await.is_ok() {
                this.popup.toggle();
            }
        });

        tracing::info!(shortcut = %registration.describe(), "shortcut");
        if let Some(message) = registration.user_message() {
            // §3/§4: tell the user rather than letting the key silently not work.
            notify::shortcut_failed(&message);
        }
        *self.registration.borrow_mut() = registration;
    }

    fn start_tray(self: &Rc<Self>, _paused: bool) {
        let settings = self.settings();
        if !settings.general.show_tray_icon {
            return;
        }

        let Some(handle) = tray::start(settings.privacy.paused) else {
            return;
        };
        let commands = handle.commands();
        *self.tray.borrow_mut() = Some(handle);

        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            while let Ok(command) = commands.recv().await {
                this.handle_tray(command);
            }
        });
    }

    /// Run `action` on a fresh main-loop callback rather than inline.
    ///
    /// **This is not optional, and it is not about tidiness.** Every handler in
    /// this file is reached from inside a [`glib::spawn_future_local`] future.
    /// A GTK modal dialog runs a *nested* main loop, that loop re-polls the
    /// same futures, and glib's executor refuses to be entered twice — it
    /// panics with `EnterError` inside a C callback, which cannot unwind, so
    /// the process **aborts**. Not an error, not a hang: the whole daemon dies
    /// and takes clipboard monitoring with it.
    ///
    /// `idle_add_local_once` schedules the work outside the future being
    /// polled, so by the time a dialog opens there is no executor entered and
    /// nesting is legal. Any handler here that can reach `dialog.run()` — via
    /// [`crate::ui::dialogs`] or `Popup`'s own confirm/edit/view paths — must
    /// go through this.
    fn later(self: &Rc<Self>, action: impl FnOnce(Rc<Self>) + 'static) {
        let this = Rc::clone(self);
        glib::idle_add_local_once(move || action(this));
    }

    fn handle_tray(self: &Rc<Self>, command: TrayCommand) {
        match command {
            // These three touch no modal dialog, so they are safe inline.
            TrayCommand::Open => self.popup.show(),
            TrayCommand::Pause => self.set_paused(true),
            TrayCommand::Resume => self.set_paused(false),

            // These open dialogs, so they must be deferred — see `later`.
            TrayCommand::Clear => self.later(|this| {
                // Through the popup so the confirmation wording is the single
                // copy the panel's own button uses.
                this.popup.show();
                this.popup.request_clear_all();
            }),
            TrayCommand::Settings => self.later(|this| this.open_settings()),
            TrayCommand::About => self.later(|this| this.popup.show_about()),
            TrayCommand::Quit => self.later(|this| this.shutdown()),
        }
    }

    // ---- monitor ------------------------------------------------------------

    /// Drain clippings from the monitor thread onto the main loop.
    fn pump_monitor(self: &Rc<Self>) {
        let events = self.monitor.events();
        let this = Rc::clone(self);

        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                match event {
                    MonitorEvent::Clip(clip) => this.store_clip(*clip),
                    MonitorEvent::Dropped { reason, notify: message } => {
                        tracing::debug!(reason, "clipping not stored");
                        if let Some(message) = message {
                            notify::clipping_dropped(&message);
                        }
                    }
                    MonitorEvent::Status(status) => this.record_health(status),
                }
            }
            tracing::debug!("monitor event stream closed");
        });
    }

    fn store_clip(self: &Rc<Self>, clip: NewClip) {
        let collapse = self.settings().clipboard.ignore_duplicates;
        let outcome = self.store.borrow_mut().insert(&clip, collapse);

        match outcome {
            Ok(result) => {
                tracing::debug!(id = result.id(), new = result.is_new(), "clipping stored");
                // Only rebuild if the user can see the list; rebuilding a
                // hidden panel on every copy would be pure waste.
                if self.popup.is_visible() {
                    self.popup.refresh();
                }
            }
            Err(err) => {
                // §35: a failed insert must not take the daemon down, but the
                // user has to know history is not being kept.
                tracing::error!(error = %err, "could not store a clipping");
                notify::storage_error(
                    "An entry could not be saved to the history database. \
                     Check available disk space.",
                );
            }
        }
    }

    fn record_health(self: &Rc<Self>, status: crate::clipboard::BackendStatus) {
        let (text, problem) = match &status.health {
            Health::Running => ("running".to_string(), None),
            Health::Degraded { detail } => {
                (format!("degraded: {detail}"), Some(detail.clone()))
            }
            Health::Failed { detail } => (format!("failed: {detail}"), Some(detail.clone())),
        };
        tracing::info!(backend = status.backend, health = %text, "backend status");
        *self.health.borrow_mut() = text;

        if let Some(detail) = problem {
            notify::backend_problem(&detail);
        }
    }

    // ---- IPC ----------------------------------------------------------------

    fn pump_ipc(self: &Rc<Self>) {
        let requests = self._ipc.requests();
        let this = Rc::clone(self);

        glib::spawn_future_local(async move {
            while let Ok(pending) = requests.recv().await {
                this.handle_request(pending);
            }
            tracing::debug!("IPC request stream closed");
        });
    }

    fn handle_request(self: &Rc<Self>, pending: Pending) {
        let response = match &pending.request {
            Request::Ping => Response::message("clipboard-manager is running"),
            Request::Toggle => {
                self.popup.toggle();
                Response::ok()
            }
            Request::Show => {
                self.popup.show();
                Response::ok()
            }
            Request::Hide => {
                self.popup.hide();
                Response::ok()
            }
            Request::Settings => {
                // Deferred for the same reason the tray's Settings item is:
                // the window's own controls can open modal dialogs, and
                // building it from inside a future puts us one click away from
                // the nested-main-loop abort described on `later`.
                self.later(|this| this.open_settings());
                Response::ok()
            }
            Request::Pause => {
                self.set_paused(true);
                Response::message("monitoring paused")
            }
            Request::Resume => {
                self.set_paused(false);
                Response::message("monitoring resumed")
            }
            Request::Clear { include_pinned } => self.clear(*include_pinned),
            Request::Status => self.status(),
            Request::Search { query, limit } => self.search(query, *limit),
            Request::Quit => {
                // Reply before shutting down, or the client sees a closed
                // connection instead of an acknowledgement.
                pending.reply(Response::message("shutting down"));
                // The local variant: this closure holds an `Rc` and must stay
                // on the main thread. Deferred so the IPC thread has written
                // the reply before the main loop stops.
                let this = Rc::clone(self);
                glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                    this.shutdown()
                });
                return;
            }
        };
        pending.reply(response);
    }

    fn clear(self: &Rc<Self>, include_pinned: bool) -> Response {
        match self.store.borrow_mut().clear(include_pinned) {
            Ok(removed) => {
                self.popup.invalidate_thumbnails();
                if self.popup.is_visible() {
                    self.popup.refresh();
                }
                notify::cleared(removed);
                Response::message(format!("{removed} entries deleted"))
            }
            Err(err) => {
                tracing::error!(error = %err, "could not clear the history");
                Response::error("the history could not be cleared")
            }
        }
    }

    fn search(&self, query: &str, limit: usize) -> Response {
        // Bounded so a CLI typo cannot ask for the whole database over a socket.
        let limit = limit.clamp(1, 200);
        let store = self.store.borrow();
        match store.search(query, limit) {
            Ok(items) => Response::Items {
                items: items
                    .into_iter()
                    .map(|item| ItemSummary {
                        id: item.id,
                        kind: item.kind.as_str().to_string(),
                        preview: item.title(),
                        size_bytes: item.size_bytes,
                        is_pinned: item.is_pinned,
                        last_used_at: item.last_used_at,
                    })
                    .collect(),
            },
            Err(err) => {
                tracing::error!(error = %err, "search failed");
                Response::error("the search could not be completed")
            }
        }
    }

    fn status(&self) -> Response {
        let settings = self.settings();
        let store = self.store.borrow();
        let stats = store.stats().unwrap_or_default();
        let registration = self.registration.borrow();

        Response::Status(Box::new(StatusReport {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            session: self.session.describe(),
            backend: self.monitor.backend_name().to_string(),
            health: self.health.borrow().clone(),
            caveat: self.caveat.clone(),
            shortcut: registration.describe(),
            shortcut_working: registration.is_working(),
            paused: settings.privacy.paused,
            total_items: stats.total_items,
            pinned_items: stats.pinned_items,
            storage_bytes: stats.total_bytes(),
            storage_path: store.data_dir().display().to_string(),
        }))
    }

    // ---- state --------------------------------------------------------------

    fn settings(&self) -> Settings {
        match self.settings.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn set_paused(self: &Rc<Self>, paused: bool) {
        {
            let mut guard = match self.settings.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if guard.privacy.paused == paused {
                return;
            }
            guard.privacy.paused = paused;
            if let Err(err) = guard.save() {
                tracing::warn!(error = %err, "could not persist the paused state");
            }
        }

        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.set_paused(paused);
        }
        if paused {
            notify::paused();
        } else {
            notify::resumed();
        }
    }

    fn open_settings(self: &Rc<Self>) {
        let mut slot = self.settings_window.borrow_mut();
        if let Some(window) = slot.as_ref() {
            window.present();
            return;
        }

        let this = Rc::clone(self);
        let window = SettingsWindow::new(settings_window::Deps {
            settings: self.settings.clone(),
            store: Rc::clone(&self.store),
            on_changed: Rc::new({
                let this = Rc::clone(&this);
                move || {
                    this.popup.invalidate_thumbnails();
                    if this.popup.is_visible() {
                        this.popup.refresh();
                    }
                }
            }),
            on_shortcut_changed: Rc::new({
                let this = Rc::clone(&this);
                move |spec: String| this.register_shortcut(&spec)
            }),
            session: self.session.describe(),
            backend: self.monitor.backend_name().to_string(),
            caveat: self.caveat.clone(),
        });
        window.present();
        *slot = Some(window);
    }

    /// Periodic expiry, blob collection and vacuum (§48).
    fn schedule_maintenance(self: &Rc<Self>) {
        let this = Rc::clone(self);
        glib::timeout_add_local_once(MAINTENANCE_FIRST_DELAY, move || {
            this.run_maintenance();
            let repeating = Rc::clone(&this);
            glib::timeout_add_local(MAINTENANCE_INTERVAL, move || {
                repeating.run_maintenance();
                glib::ControlFlow::Continue
            });
        });
    }

    fn run_maintenance(self: &Rc<Self>) {
        let settings = self.settings();
        let report = self.store.borrow_mut().maintain(&settings);
        match report {
            Ok(report) if report.did_anything() => {
                tracing::info!(?report, "maintenance");
                if self.popup.is_visible() {
                    self.popup.refresh();
                }
            }
            Ok(_) => {}
            Err(err) => tracing::error!(error = %err, "maintenance failed"),
        }
    }

    /// Clean shutdown: honour "clear on exit", then quit the main loop (§49).
    pub fn shutdown(self: &Rc<Self>) {
        tracing::info!("shutting down");

        let settings = self.settings();
        if settings.privacy.clear_on_exit {
            let include_pinned = settings.privacy.clear_pinned_on_exit;
            match self.store.borrow_mut().clear(include_pinned) {
                Ok(removed) => tracing::info!(removed, include_pinned, "cleared history on exit"),
                Err(err) => tracing::error!(error = %err, "could not clear history on exit"),
            }
        }

        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.shutdown();
        }
        self.monitor.stop();
        self.popup.hide();

        gtk::main_quit();
    }

    pub fn popup(&self) -> &Rc<Popup> {
        &self.popup
    }
}

/// Refuse to run as root (§28).
fn refuse_to_run_as_root() -> Result<()> {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        bail!(
            "clipboard-manager must not run as root. It is a per-user service: run it as \
             your own user, without sudo. Running as root would store one user's clipboard \
             in a root-owned database and expose it across accounts."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_as_root_is_refused_and_anything_else_is_allowed() {
        // Asserts the *logic* against the real uid rather than asserting what
        // the uid is. CI runs these in containers as root, where demanding a
        // non-root uid would fail for reasons that say nothing about the code.
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let euid = unsafe { libc::geteuid() };
        let outcome = refuse_to_run_as_root();

        if euid == 0 {
            let err = outcome.expect_err("root must be refused (§28)").to_string();
            assert!(err.contains("must not run as root"), "{err}");
            // The message has to tell the user what to do instead.
            assert!(err.contains("sudo"), "{err}");
        } else {
            assert!(outcome.is_ok(), "a normal user must be allowed to run the daemon");
        }
    }

    #[test]
    fn maintenance_is_deferred_so_startup_stays_fast() {
        // §46: heavy work must not happen during launch.
        assert!(MAINTENANCE_FIRST_DELAY >= std::time::Duration::from_secs(30));
        assert!(MAINTENANCE_INTERVAL >= std::time::Duration::from_secs(600));
    }
}
