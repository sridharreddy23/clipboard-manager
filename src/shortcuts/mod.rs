//! Global shortcut registration (§3, §4).
//!
//! The default is **`Ctrl + \``** — Ctrl together with the backtick/grave key
//! above Tab. Not `Super+V`, not `Ctrl+Shift+\``. It is configurable, and the
//! single source of truth for the default is
//! [`crate::config::DEFAULT_SHORTCUT`].
//!
//! # The ladder
//!
//! There is no one way to register a global shortcut on Linux, so we try in
//! order of reliability and stop at the first that genuinely works:
//!
//! | Order | Mechanism | Where it applies |
//! |---|---|---|
//! | 1 | [`x11`] `XGrabKey` on the root window | any X11 session |
//! | 2 | [`desktop`] GSettings custom keybinding | GNOME, Cinnamon (Wayland or X11) |
//! | 3 | [`desktop`] xfconf | XFCE |
//! | 4 | manual instructions | everything else, including KDE and wlroots |
//!
//! Step 1 is preferred on X11 because it needs no desktop cooperation and
//! responds in-process. Steps 2 and 3 hand the binding to the desktop, which
//! runs `clipboard-manager toggle`; that reaches the daemon over its socket, so
//! the behaviour is identical from the user's point of view.
//!
//! # Honesty
//!
//! §3: *"Do not silently claim that the shortcut works when it does not."*
//! [`Registration`] therefore distinguishes three outcomes, and the
//! [`Registration::Unavailable`] case carries the reason and the exact command
//! to bind by hand. The daemon surfaces that as a notification and in
//! `clipboard-manager status`; nothing pretends to have succeeded.
//!
//! An X11 grab is never attempted on a Wayland session even though XWayland
//! would accept it, because the grab only receives events while an X11 client
//! has focus — it would look registered and then not fire for native Wayland
//! applications, which is precisely the failure mode §3 forbids.

pub mod desktop;
pub mod keys;
pub mod x11;

use crate::platform::{Desktop, DisplayServer, Session};
use keys::Accelerator;

/// What happened when we tried to register.
pub enum Registration {
    /// We hold the key ourselves and will invoke the callback directly.
    Direct {
        /// Kept alive for as long as the grab should last; dropping releases it.
        grab: x11::Grab,
        accelerator: Accelerator,
    },
    /// The desktop holds the binding and will run `clipboard-manager toggle`.
    Delegated { detail: String, accelerator: Accelerator },
    /// Nothing could be registered. The user has to bind it themselves.
    Unavailable { reason: String, instructions: String, accelerator: Accelerator },
}

impl Registration {
    pub fn accelerator(&self) -> &Accelerator {
        match self {
            Registration::Direct { accelerator, .. }
            | Registration::Delegated { accelerator, .. }
            | Registration::Unavailable { accelerator, .. } => accelerator,
        }
    }

    /// True only when the shortcut will actually open the panel.
    pub fn is_working(&self) -> bool {
        !matches!(self, Registration::Unavailable { .. })
    }

    /// One line for `clipboard-manager status` and the About dialog.
    pub fn describe(&self) -> String {
        match self {
            Registration::Direct { grab, accelerator } => {
                format!("{} — {}", accelerator.to_display(), grab.detail())
            }
            Registration::Delegated { detail, accelerator } => {
                format!("{} — {detail}", accelerator.to_display())
            }
            Registration::Unavailable { reason, accelerator, .. } => {
                format!("{} — NOT REGISTERED: {reason}", accelerator.to_display())
            }
        }
    }

    /// Message to show the user, or `None` when there is nothing they need to
    /// do. Only the failure case produces one: a notification on every
    /// successful start would be noise (§36).
    pub fn user_message(&self) -> Option<String> {
        match self {
            Registration::Unavailable { reason, instructions, .. } => {
                Some(format!("{reason}\n\n{instructions}"))
            }
            _ => None,
        }
    }
}

/// Register the configured shortcut, walking the ladder.
///
/// `on_press` is only used by the direct X11 path; the delegated paths reach the
/// daemon through the CLI instead. It is always invoked on the GTK main thread.
pub fn register<F>(session: &Session, spec: &str, on_press: F) -> Registration
where
    F: Fn() + Send + Sync + 'static,
{
    let accelerator = match Accelerator::parse(spec) {
        Ok(accelerator) => accelerator,
        Err(err) => {
            // A bad accelerator in the config file must not stop the daemon;
            // fall back to the shipped default so the user still has a working
            // shortcut, and say what happened.
            tracing::warn!(spec, error = %err, "unusable shortcut in settings; using the default");
            match Accelerator::parse(crate::config::DEFAULT_SHORTCUT) {
                Ok(fallback) => fallback,
                Err(fatal) => {
                    // Unreachable unless the constant itself is broken, which a
                    // unit test prevents.
                    return Registration::Unavailable {
                        reason: format!("the shortcut {spec:?} could not be understood: {err}"),
                        instructions: format!("internal error: {fatal}"),
                        accelerator: Accelerator {
                            mods: keys::Modifiers::default(),
                            key: "grave".to_string(),
                        },
                    };
                }
            }
        }
    };

    // -- 1. X11 direct grab --
    if session.server == DisplayServer::X11 {
        match x11::grab(&accelerator, on_press) {
            Ok(grab) => {
                tracing::info!(
                    shortcut = %accelerator.to_display(),
                    "global shortcut grabbed from the X server"
                );
                return Registration::Direct { grab, accelerator };
            }
            Err(err) => {
                tracing::warn!(error = %err, "could not grab the shortcut directly");
                // Fall through: the desktop may still be able to register it.
            }
        }
    }

    // -- 2/3. hand it to the desktop --
    match desktop::register(&session.desktop, &accelerator) {
        Ok(detail) => {
            tracing::info!(
                shortcut = %accelerator.to_display(),
                desktop = %session.desktop,
                "global shortcut registered with the desktop"
            );
            return Registration::Delegated { detail, accelerator };
        }
        Err(err) => {
            tracing::info!(
                desktop = %session.desktop,
                error = %err,
                "the desktop cannot register the shortcut for us"
            );
        }
    }

    // -- 4. manual --
    let reason = unavailable_reason(session);
    let instructions = desktop::manual_instructions(&session.desktop, &accelerator);
    tracing::warn!(
        shortcut = %accelerator.to_display(),
        "the global shortcut could not be registered; manual setup is needed"
    );
    Registration::Unavailable { reason, instructions, accelerator }
}

/// Remove a previously delegated binding, so changing the shortcut does not
/// leave the old key bound.
pub fn unregister(session: &Session) {
    if let Err(err) = desktop::unregister(&session.desktop) {
        tracing::debug!(error = %err, "could not remove the previous shortcut binding");
    }
}

/// Why registration failed, phrased for the person reading the notification
/// rather than for a developer.
fn unavailable_reason(session: &Session) -> String {
    match (session.server, &session.desktop) {
        (DisplayServer::Wayland, Desktop::Kde) => {
            "Plasma only applies keyboard shortcuts written by its own settings \
             application, so the shortcut has to be added there once."
                .to_string()
        }
        (DisplayServer::Wayland, desktop)
            if matches!(
                desktop,
                Desktop::Sway | Desktop::Hyprland | Desktop::Wayfire | Desktop::River
            ) =>
        {
            format!(
                "{} takes its key bindings from its configuration file, so the shortcut \
                 has to be added there.",
                desktop.label()
            )
        }
        (DisplayServer::Wayland, _) => {
            "Wayland does not let an application claim a key for itself, and this \
             desktop offers no way to register one on its behalf."
                .to_string()
        }
        (DisplayServer::X11, _) => {
            "The shortcut could not be claimed — another application is most likely \
             already using it."
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Desktop;

    fn session(server: DisplayServer, desktop: Desktop) -> Session {
        Session {
            server,
            desktop,
            wayland_display: match server {
                DisplayServer::Wayland => Some("wayland-0".to_string()),
                DisplayServer::X11 => None,
            },
            x11_display: Some(":0".to_string()),
        }
    }

    #[test]
    fn a_failed_registration_is_reported_as_not_working() {
        let accelerator = Accelerator::parse("Ctrl+grave").unwrap();
        let registration = Registration::Unavailable {
            reason: "nope".to_string(),
            instructions: "do it yourself".to_string(),
            accelerator,
        };
        assert!(!registration.is_working());
        // §3: the status line must not read like success.
        assert!(registration.describe().contains("NOT REGISTERED"));
        assert!(registration.user_message().is_some());
    }

    #[test]
    fn a_delegated_registration_does_not_nag_the_user() {
        let accelerator = Accelerator::parse("Ctrl+grave").unwrap();
        let registration = Registration::Delegated {
            detail: "registered with the desktop".to_string(),
            accelerator,
        };
        assert!(registration.is_working());
        // A notification on every successful start would be noise (§36).
        assert_eq!(registration.user_message(), None);
        assert!(registration.describe().contains("Ctrl + ` (backtick)"));
    }

    #[test]
    fn the_failure_explanation_names_the_actual_obstacle() {
        // Each of these phrasings tells the user something they can act on,
        // rather than "registration failed".
        let kde = unavailable_reason(&session(DisplayServer::Wayland, Desktop::Kde));
        assert!(kde.contains("Plasma"), "{kde}");

        let sway = unavailable_reason(&session(DisplayServer::Wayland, Desktop::Sway));
        assert!(sway.contains("configuration file"), "{sway}");

        let x11 = unavailable_reason(&session(DisplayServer::X11, Desktop::Xfce));
        assert!(x11.contains("already using it"), "{x11}");

        let other = unavailable_reason(&session(DisplayServer::Wayland, Desktop::Mate));
        assert!(other.contains("Wayland does not let"), "{other}");
    }

    #[test]
    fn an_unparseable_configured_shortcut_falls_back_to_the_shipped_default() {
        // The daemon must still start, and the user must still have a working
        // key, if `config.toml` holds nonsense.
        let session = session(DisplayServer::Wayland, Desktop::Unknown("none".into()));
        let registration = register(&session, "this is not a shortcut", || {});
        assert_eq!(registration.accelerator().key, "grave");
        assert!(registration.accelerator().mods.ctrl);
    }

    #[test]
    fn registration_on_an_unknown_wayland_desktop_is_honest_about_failing() {
        // No X11 grab attempted, no desktop mechanism available: the only
        // correct outcome is Unavailable with instructions.
        let session = session(DisplayServer::Wayland, Desktop::Unknown("nothing".into()));
        let registration = register(&session, "Ctrl+grave", || {});
        assert!(!registration.is_working());
        let message = registration.user_message().expect("must tell the user what to do");
        assert!(message.contains("toggle"), "{message}");
    }
}
