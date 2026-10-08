//! Display server and desktop detection, and the per-platform backends
//! (§18, §19, §20, §21).
//!
//! Everything above this module is display-server agnostic. The abstraction
//! boundary is deliberately narrow: a backend watches for selection changes and
//! hands [`crate::model::RawClip`]s to the pipeline, and that is all. Setting
//! the clipboard is *not* here — it goes through GTK on the main thread (see
//! [`crate::ui::selection`]), because GDK already implements selection
//! ownership, INCR transfers and `wl_data_source` correctly on both display
//! servers, and reimplementing that would be strictly worse.
//!
//! # Which backend runs where
//!
//! | Session | Backend | How |
//! |---|---|---|
//! | X11 (any desktop) | [`x11`] | XFIXES selection notifications |
//! | Wayland, wlroots-based (Sway, Hyprland, Wayfire, river) | [`wayland`] | `zwlr_data_control_manager_v1` |
//! | Wayland, KDE Plasma ≥ 5.25 | [`wayland`] | `zwlr_data_control_manager_v1` |
//! | Wayland, GNOME | [`x11`] via XWayland | mutter keeps the X11 CLIPBOARD in sync |
//!
//! The last row needs explaining, because it is the one real gap. Wayland has
//! no core protocol that lets an ordinary client observe the clipboard —
//! that is a deliberate security property, not an oversight. wlroots-based
//! compositors and KWin implement `zwlr_data_control_manager_v1` to let
//! clipboard managers opt in; mutter implements no equivalent. So under GNOME
//! Wayland we connect as an ordinary XWayland client and watch the X11
//! CLIPBOARD, which mutter keeps synchronised with the Wayland one in both
//! directions.
//!
//! What that costs is stated plainly rather than papered over: clipboard
//! changes made between two native-Wayland applications are observed, because
//! mutter mirrors them into X11, but the owning application cannot be
//! identified, so application exclusion does not work there. This does **not**
//! require running the desktop under XWayland, does not require disabling any
//! Wayland security measure, and does not require the user to change their
//! session type.

pub mod wake;
pub mod wayland;
pub mod x11;

use std::fmt;

use anyhow::{bail, Result};

use crate::clipboard::BackendFactory;

/// The display server this session is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayServer {
    X11,
    Wayland,
}

impl DisplayServer {
    pub fn as_str(self) -> &'static str {
        match self {
            DisplayServer::X11 => "X11",
            DisplayServer::Wayland => "Wayland",
        }
    }
}

/// The desktop environment, used only to pick the right global-shortcut
/// mechanism and tray behaviour (§21). Core functionality never branches on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Desktop {
    Gnome,
    Kde,
    Xfce,
    Cinnamon,
    Mate,
    Lxqt,
    Sway,
    Hyprland,
    Wayfire,
    River,
    Unknown(String),
}

impl Desktop {
    /// Parse `XDG_CURRENT_DESKTOP` and friends.
    ///
    /// `XDG_CURRENT_DESKTOP` is colon-separated and ordered most-specific
    /// first (`ubuntu:GNOME`), so each component is tried in turn.
    pub fn from_env() -> Self {
        let candidates = [
            std::env::var("XDG_CURRENT_DESKTOP").ok(),
            std::env::var("XDG_SESSION_DESKTOP").ok(),
            std::env::var("DESKTOP_SESSION").ok(),
        ];

        for value in candidates.iter().flatten() {
            for part in value.split(':') {
                if let Some(desktop) = Self::parse(part) {
                    return desktop;
                }
            }
        }

        // Some compositors only identify themselves through their own variables.
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            return Desktop::Hyprland;
        }
        if std::env::var_os("SWAYSOCK").is_some() {
            return Desktop::Sway;
        }

        Desktop::Unknown(
            candidates
                .into_iter()
                .flatten()
                .next()
                .unwrap_or_else(|| "unknown".to_string()),
        )
    }

    fn parse(name: &str) -> Option<Self> {
        let lower = name.trim().to_lowercase();
        Some(match lower.as_str() {
            "gnome" | "gnome-classic" | "gnome-flashback" | "ubuntu" | "pop" | "unity" => {
                Desktop::Gnome
            }
            "kde" | "plasma" | "plasma5" | "plasma6" => Desktop::Kde,
            "xfce" | "xfce4" | "xubuntu" => Desktop::Xfce,
            "x-cinnamon" | "cinnamon" => Desktop::Cinnamon,
            "mate" => Desktop::Mate,
            "lxqt" => Desktop::Lxqt,
            "sway" => Desktop::Sway,
            "hyprland" => Desktop::Hyprland,
            "wayfire" => Desktop::Wayfire,
            "river" => Desktop::River,
            _ => return None,
        })
    }

    pub fn label(&self) -> &str {
        match self {
            Desktop::Gnome => "GNOME",
            Desktop::Kde => "KDE Plasma",
            Desktop::Xfce => "Xfce",
            Desktop::Cinnamon => "Cinnamon",
            Desktop::Mate => "MATE",
            Desktop::Lxqt => "LXQt",
            Desktop::Sway => "Sway",
            Desktop::Hyprland => "Hyprland",
            Desktop::Wayfire => "Wayfire",
            Desktop::River => "river",
            Desktop::Unknown(name) => name,
        }
    }

    /// True for the desktops known to implement `zwlr_data_control_manager_v1`.
    ///
    /// Only used to decide which backend to *try first*; availability is always
    /// confirmed by actually looking for the global, never assumed from this.
    pub fn likely_has_data_control(&self) -> bool {
        matches!(
            self,
            Desktop::Kde
                | Desktop::Sway
                | Desktop::Hyprland
                | Desktop::Wayfire
                | Desktop::River
                | Desktop::Lxqt
        )
    }
}

impl fmt::Display for Desktop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Everything we detected about the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub server: DisplayServer,
    pub desktop: Desktop,
    /// Value of `WAYLAND_DISPLAY`, when set.
    pub wayland_display: Option<String>,
    /// Value of `DISPLAY`, when set. Present under Wayland too, whenever
    /// XWayland is running.
    pub x11_display: Option<String>,
}

impl Session {
    /// Detect from the environment.
    ///
    /// `XDG_SESSION_TYPE` is authoritative when it is set and consistent with
    /// the socket variables; otherwise the presence of `WAYLAND_DISPLAY` wins,
    /// because a Wayland session with a stale `XDG_SESSION_TYPE=x11` is a real
    /// configuration seen in the wild and getting this backwards would mean
    /// choosing the wrong backend.
    pub fn detect() -> Self {
        Self::from_env(
            std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
            std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
            std::env::var("DISPLAY").ok().as_deref(),
            Desktop::from_env(),
        )
    }

    /// Detection logic, separated from the environment so it can be tested.
    pub fn from_env(
        session_type: Option<&str>,
        wayland_display: Option<&str>,
        x11_display: Option<&str>,
        desktop: Desktop,
    ) -> Self {
        let wayland_display = wayland_display.filter(|s| !s.is_empty()).map(str::to_string);
        let x11_display = x11_display.filter(|s| !s.is_empty()).map(str::to_string);

        let server = if wayland_display.is_some() {
            DisplayServer::Wayland
        } else if x11_display.is_some() {
            DisplayServer::X11
        } else if session_type.is_some_and(|t| t.eq_ignore_ascii_case("wayland")) {
            DisplayServer::Wayland
        } else {
            DisplayServer::X11
        };

        Self { server, desktop, wayland_display, x11_display }
    }

    /// True when this is a Wayland session that also has XWayland available.
    pub fn has_xwayland(&self) -> bool {
        self.server == DisplayServer::Wayland && self.x11_display.is_some()
    }

    /// One-line description for `status` output and the About dialog.
    pub fn describe(&self) -> String {
        format!("{} on {}", self.desktop.label(), self.server.as_str())
    }
}

/// A chosen monitoring backend, ready to hand to [`crate::clipboard::Monitor`].
pub struct ChosenBackend {
    pub name: &'static str,
    pub factory: BackendFactory,
    /// Limitation to surface to the user, if the chosen path has one.
    pub caveat: Option<String>,
}

// Hand-written because `factory` is a closure, which has no `Debug`. Worth
// having: it is what lets `choose_backend(..).unwrap_err()` be used in tests
// and what makes a chosen backend printable in `--debug` output.
impl fmt::Debug for ChosenBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChosenBackend")
            .field("name", &self.name)
            .field("caveat", &self.caveat)
            .finish_non_exhaustive()
    }
}

/// Pick the monitoring backend for this session.
///
/// Availability is *probed*, never assumed: the Wayland path is only chosen
/// after confirming that the compositor actually advertises
/// `zwlr_data_control_manager_v1`. §56 forbids claiming a platform feature
/// works when it does not, and this is where that is enforced.
pub fn choose_backend(session: &Session) -> Result<ChosenBackend> {
    if session.server == DisplayServer::Wayland {
        match wayland::probe() {
            Ok(true) => {
                tracing::info!("using the Wayland data-control backend");
                return Ok(ChosenBackend {
                    name: wayland::BACKEND_NAME,
                    factory: Box::new(wayland::build),
                    caveat: Some(
                        "Wayland does not identify the application that owns a \
                         selection, so the application exclusion list cannot be \
                         applied."
                            .to_string(),
                    ),
                });
            }
            Ok(false) => {
                tracing::info!(
                    desktop = %session.desktop,
                    "compositor does not offer wlr-data-control; falling back to XWayland"
                );
            }
            Err(err) => {
                tracing::warn!(error = %err, "could not query Wayland globals");
            }
        }

        if session.x11_display.is_some() {
            return Ok(ChosenBackend {
                name: x11::BACKEND_NAME,
                factory: Box::new(x11::build),
                caveat: Some(
                    "This compositor provides no Wayland protocol for reading the \
                     clipboard, so history is collected through XWayland. Clipboard \
                     changes are captured, but the application that owns a selection \
                     cannot be identified and the exclusion list cannot be applied."
                        .to_string(),
                ),
            });
        }

        bail!(
            "this Wayland compositor ({}) offers neither wlr-data-control nor XWayland, \
             so the clipboard cannot be monitored",
            session.desktop
        );
    }

    if session.x11_display.is_none() {
        bail!("no DISPLAY or WAYLAND_DISPLAY is set; there is no session to monitor");
    }

    Ok(ChosenBackend {
        name: x11::BACKEND_NAME,
        factory: Box::new(x11::build),
        caveat: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_display_decides_even_against_a_stale_session_type() {
        let s = Session::from_env(Some("x11"), Some("wayland-0"), Some(":0"), Desktop::Gnome);
        assert_eq!(s.server, DisplayServer::Wayland);
        assert!(s.has_xwayland());
    }

    #[test]
    fn plain_x11_is_detected() {
        let s = Session::from_env(Some("x11"), None, Some(":0"), Desktop::Xfce);
        assert_eq!(s.server, DisplayServer::X11);
        assert!(!s.has_xwayland());
        assert_eq!(s.describe(), "Xfce on X11");
    }

    #[test]
    fn empty_variables_are_treated_as_unset() {
        let s = Session::from_env(Some("wayland"), Some(""), Some(""), Desktop::Gnome);
        assert_eq!(s.server, DisplayServer::Wayland);
        assert_eq!(s.wayland_display, None);
        assert_eq!(s.x11_display, None);
    }

    #[test]
    fn a_wayland_session_without_xwayland_has_no_x11_fallback() {
        let s = Session::from_env(Some("wayland"), Some("wayland-0"), None, Desktop::Gnome);
        assert!(!s.has_xwayland());
    }

    #[test]
    fn desktop_names_are_parsed_from_the_xdg_variable_form() {
        assert_eq!(Desktop::parse("GNOME"), Some(Desktop::Gnome));
        assert_eq!(Desktop::parse("ubuntu"), Some(Desktop::Gnome));
        assert_eq!(Desktop::parse("KDE"), Some(Desktop::Kde));
        assert_eq!(Desktop::parse("X-Cinnamon"), Some(Desktop::Cinnamon));
        assert_eq!(Desktop::parse("  xfce4 "), Some(Desktop::Xfce));
        assert_eq!(Desktop::parse("MATE"), Some(Desktop::Mate));
        assert_eq!(Desktop::parse("Hyprland"), Some(Desktop::Hyprland));
        assert_eq!(Desktop::parse("something-else"), None);
    }

    #[test]
    fn data_control_expectations_match_the_documented_compositors() {
        assert!(Desktop::Kde.likely_has_data_control());
        assert!(Desktop::Sway.likely_has_data_control());
        assert!(!Desktop::Gnome.likely_has_data_control());
        assert!(!Desktop::Unknown("foo".into()).likely_has_data_control());
    }

    #[test]
    fn a_headless_session_is_an_error_rather_than_a_silent_no_op() {
        let s = Session::from_env(None, None, None, Desktop::Unknown("none".into()));
        let err = choose_backend(&s).unwrap_err().to_string();
        assert!(err.contains("no session to monitor"), "{err}");
    }

    #[test]
    fn x11_sessions_get_the_x11_backend_with_no_caveat() {
        let s = Session::from_env(Some("x11"), None, Some(":0"), Desktop::Kde);
        let chosen = choose_backend(&s).unwrap();
        assert_eq!(chosen.name, x11::BACKEND_NAME);
        assert!(chosen.caveat.is_none());
    }
}
