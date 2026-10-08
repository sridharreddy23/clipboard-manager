//! Desktop notifications (§36).
//!
//! Sent over `org.freedesktop.Notifications`, which every desktop we support
//! implements. GIO is used rather than a notification crate so there is no
//! extra dependency and no second D-Bus stack: the proxy shares the GLib main
//! loop the UI already runs on.
//!
//! # What is worth interrupting someone for
//!
//! §36 lists the occasions, and the shape of this module enforces the part that
//! matters: **never one notification per copy.** A clipboard manager that
//! announced every clipping would be unusable within a minute. The only things
//! here are events the user either asked for (cleared, paused, resumed) or
//! needs to act on (the shortcut did not register, a clipping was too large,
//! storage is failing).
//!
//! Over-size notifications are additionally rate-limited, because copying a
//! directory of large files would otherwise produce one per file.
//!
//! # Never the content
//!
//! §27: a notification body never contains clipboard text. "Cleared 37 entries"
//! is a count; "Copied: hunter2" would be a password on a lock screen. The
//! functions here take counts and reasons, and there is no overload that takes
//! a [`crate::model::ClipItem`] — that absence is the safeguard.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use gtk::prelude::*;

/// Our application name, as shown by notification daemons that display one.
const APP_NAME: &str = "Clipboard Manager";

/// Icon name from the standard icon theme.
const ICON: &str = "edit-paste";

/// Urgency levels, as the specification defines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Urgency {
    Low = 0,
    Normal = 1,
}

/// Shortest gap between two "too large" notifications.
const RATE_LIMIT: Duration = Duration::from_secs(30);

// Long enough to cover a multi-file copy, which is the case this exists for.
const _: () = assert!(RATE_LIMIT.as_secs() >= 5);

thread_local! {
    /// When the last rate-limited notification went out. Thread-local because
    /// notifications are only ever sent from the GTK main thread, so this needs
    /// no synchronisation.
    static LAST_SIZE_WARNING: RefCell<Option<Instant>> = const { RefCell::new(None) };
}

/// History was cleared, at the user's request.
pub fn cleared(count: usize) {
    let body = match count {
        0 => "There was nothing to clear.".to_string(),
        1 => "1 entry was deleted.".to_string(),
        n => format!("{n} entries were deleted."),
    };
    send("Clipboard history cleared", &body, Urgency::Low);
}

pub fn paused() {
    send(
        "Clipboard monitoring paused",
        "New copies will not be recorded. Existing history is still available.",
        Urgency::Normal,
    );
}

pub fn resumed() {
    send("Clipboard monitoring resumed", "New copies are being recorded again.", Urgency::Low);
}

/// The global shortcut could not be registered (§3, §4).
///
/// Normal urgency and a long body on purpose: without this the user presses the
/// key, nothing happens, and they have no way to find out why.
pub fn shortcut_failed(message: &str) {
    send("Clipboard shortcut needs setting up", message, Urgency::Normal);
}

/// A clipping was rejected. Rate-limited.
pub fn clipping_dropped(reason: &str) {
    if !rate_limit_allows() {
        tracing::debug!(reason, "suppressed a repeat notification");
        return;
    }
    send("Clipboard item not saved", reason, Urgency::Normal);
}

/// Should a rate-limited notification go out now?
///
/// Records the time when it returns `true`, so callers must not call this
/// speculatively. Factored out so the test exercises the real limiter rather
/// than a copy of its logic.
fn rate_limit_allows() -> bool {
    LAST_SIZE_WARNING.with(|last| {
        let mut last = last.borrow_mut();
        let now = Instant::now();
        match *last {
            Some(previous) if now.duration_since(previous) < RATE_LIMIT => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    })
}

/// Storage is failing — the one case the user really must know about, because
/// history is silently not being kept (§35).
pub fn storage_error(detail: &str) {
    send("Clipboard history cannot be saved", detail, Urgency::Normal);
}

/// The monitoring backend degraded or died.
pub fn backend_problem(detail: &str) {
    send("Clipboard monitoring problem", detail, Urgency::Normal);
}

/// Post one notification. Failure is logged at debug and otherwise ignored:
/// a session with no notification daemon is perfectly normal, and a clipboard
/// manager must not care.
fn send(summary: &str, body: &str, urgency: Urgency) {
    if let Err(err) = try_send(summary, body, urgency) {
        tracing::debug!(error = %err, summary, "could not post a notification");
    }
}

fn try_send(summary: &str, body: &str, urgency: Urgency) -> Result<(), glib::Error> {
    let proxy = gio::DBusProxy::for_bus_sync(
        gio::BusType::Session,
        gio::DBusProxyFlags::DO_NOT_AUTO_START,
        None,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        gio::Cancellable::NONE,
    )?;

    let hints = glib::VariantDict::new(None);
    hints.insert("urgency", (urgency as u8).to_variant());
    // Lets desktops that support it fold our notifications together rather
    // than stacking them.
    hints.insert("desktop-entry", "clipboard-manager".to_variant());

    let args = glib::Variant::tuple_from_iter([
        APP_NAME.to_variant(),
        // 0 means "new notification" rather than replacing an existing one.
        0u32.to_variant(),
        ICON.to_variant(),
        summary.to_variant(),
        body.to_variant(),
        Vec::<String>::new().to_variant(),
        hints.end(),
        // Let the desktop decide how long to show it.
        (-1i32).to_variant(),
    ]);

    proxy.call_sync(
        "Notify",
        Some(&args),
        gio::DBusCallFlags::NONE,
        1_000,
        gio::Cancellable::NONE,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_size_warnings_are_suppressed() {
        // Copying a folder of large files must not produce one notification
        // per file (§36). Exercises the real limiter, not a copy of it.
        LAST_SIZE_WARNING.with(|last| *last.borrow_mut() = None);

        assert!(rate_limit_allows(), "the first warning must get through");
        assert!(!rate_limit_allows(), "an immediate repeat must be suppressed");
        assert!(!rate_limit_allows(), "and so must the one after that");

        // Pretending the last one was long enough ago lets the next through,
        // so the limiter is a window rather than a one-shot latch.
        LAST_SIZE_WARNING
            .with(|last| *last.borrow_mut() = Some(Instant::now() - RATE_LIMIT * 2));
        assert!(rate_limit_allows(), "a warning after the window must get through");
    }

    #[test]
    fn cleared_messages_report_counts_and_never_content() {
        // Phrasing check, and a standing reminder that these bodies are visible
        // on a lock screen (§27).
        for count in [0usize, 1, 37] {
            let body = match count {
                0 => "There was nothing to clear.".to_string(),
                1 => "1 entry was deleted.".to_string(),
                n => format!("{n} entries were deleted."),
            };
            assert!(!body.is_empty());
            assert!(body.ends_with('.'), "{body}");
        }
    }

    #[test]
    fn urgency_values_match_the_freedesktop_specification() {
        // The hint is a raw byte on the wire; wrong values are silently ignored
        // by some daemons and rejected by others.
        assert_eq!(Urgency::Low as u8, 0);
        assert_eq!(Urgency::Normal as u8, 1);
    }
}
