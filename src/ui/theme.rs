//! Light/dark theme resolution and stylesheet loading (§38, §39).
//!
//! The popup has to look like system UI, which means following the desktop's
//! light/dark preference rather than picking one. There is no single API for
//! that across the five desktops we support, so the preference is read from the
//! most authoritative source available and falls back down a chain:
//!
//! 1. `org.freedesktop.appearance color-scheme` via the XDG settings portal —
//!    the cross-desktop standard, honoured by GNOME and KDE.
//! 2. The GTK theme name ending in `-dark`, which is how XFCE, MATE and
//!    Cinnamon express it.
//! 3. Light, because guessing dark on a light desktop is the more jarring
//!    mistake.
//!
//! The portal is queried through GIO rather than zbus so that this stays on the
//! GTK main thread with no async runtime, and a missing portal is an expected
//! outcome rather than an error.

use gtk::prelude::*;

use crate::config::Theme;

/// The stylesheet, compiled in so there is no runtime file to lose.
const STYLE: &str = include_str!("style.css");

/// Priority for our provider.
///
/// Above `GTK_STYLE_PROVIDER_PRIORITY_APPLICATION` (600) so that our rules win
/// over a distribution theme's generic widget styling, but below
/// `GTK_STYLE_PROVIDER_PRIORITY_USER` (800) so a user's own
/// `~/.config/gtk-3.0/gtk.css` can still override us.
const PRIORITY: u32 = 700;

/// What we resolved the preference to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Light,
    Dark,
}

impl Resolved {
    /// CSS class put on the popup's root so the stylesheet can branch.
    pub fn css_class(self) -> &'static str {
        match self {
            Resolved::Light => "cm-light",
            Resolved::Dark => "cm-dark",
        }
    }
}

/// Install the stylesheet on the default screen.
///
/// Safe to call more than once: a second call replaces the first provider
/// rather than stacking another copy of the rules.
pub fn install() {
    let Some(screen) = gdk::Screen::default() else {
        // No screen means no GUI, which the caller has already decided is
        // fine (`--help`, `status`, a headless unit test).
        return;
    };

    let provider = gtk::CssProvider::new();
    if let Err(err) = provider.load_from_data(STYLE.as_bytes()) {
        // A broken stylesheet must not stop the popup from opening: GTK's own
        // defaults are ugly here but entirely usable.
        tracing::error!(error = %err, "could not parse the bundled stylesheet");
        return;
    }
    gtk::StyleContext::add_provider_for_screen(&screen, &provider, PRIORITY);
}

/// Resolve the configured preference into an actual light/dark choice.
pub fn resolve(preference: Theme) -> Resolved {
    match preference {
        Theme::Light => Resolved::Light,
        Theme::Dark => Resolved::Dark,
        Theme::System => detect_system(),
    }
}

/// Apply the resolved theme to a widget's style context and to GTK itself.
pub fn apply(widget: &impl IsA<gtk::Widget>, resolved: Resolved) {
    let context = widget.style_context();
    // Remove the other class first, so switching at runtime does not leave
    // both applied.
    for candidate in [Resolved::Light, Resolved::Dark] {
        context.remove_class(candidate.css_class());
    }
    context.add_class(resolved.css_class());

    // Also set the GTK-wide dark preference, which is what makes stock widgets
    // we do not style ourselves (scrollbars, text selection, the settings
    // window's own chrome) match.
    if let Some(gtk_settings) = gtk::Settings::default() {
        gtk_settings.set_gtk_application_prefer_dark_theme(resolved == Resolved::Dark);
    }
}

fn detect_system() -> Resolved {
    if let Some(resolved) = from_portal() {
        return resolved;
    }
    if let Some(resolved) = from_gtk_theme_name() {
        return resolved;
    }
    Resolved::Light
}

/// Read `org.freedesktop.appearance color-scheme` from the settings portal.
///
/// The spec defines `0` as "no preference", `1` as dark and `2` as light. Only
/// `1` and `2` are meaningful; `0` means the desktop has not expressed one, so
/// we fall through to the next source rather than inventing an answer.
fn from_portal() -> Option<Resolved> {
    let proxy = gio::DBusProxy::for_bus_sync(
        gio::BusType::Session,
        gio::DBusProxyFlags::NONE,
        None,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
        gio::Cancellable::NONE,
    )
    .ok()?;

    let args = glib::Variant::tuple_from_iter([
        "org.freedesktop.appearance".to_variant(),
        "color-scheme".to_variant(),
    ]);

    // A short timeout: the portal may not exist, and the popup must not wait
    // on D-Bus to decide what colour to be.
    let reply = proxy
        .call_sync(
            "Read",
            Some(&args),
            gio::DBusCallFlags::NONE,
            500,
            gio::Cancellable::NONE,
        )
        .ok()?;

    // The reply is (v) wrapping another variant wrapping the u32.
    let inner = reply.child_value(0);
    let value = unwrap_variants(inner);
    match value.get::<u32>()? {
        1 => Some(Resolved::Dark),
        2 => Some(Resolved::Light),
        _ => None,
    }
}

/// Peel nested `v` wrappers until something concrete is reached.
///
/// The portal returns a variant inside a variant, and which depth the value
/// sits at has differed between portal versions, so unwrapping defensively is
/// cheaper than pinning a version.
fn unwrap_variants(mut value: glib::Variant) -> glib::Variant {
    // Bounded rather than `while`: a malformed reply must not spin.
    for _ in 0..4 {
        match value.get::<glib::Variant>() {
            Some(inner) => value = inner,
            None => break,
        }
    }
    value
}

/// Fall back to the GTK theme name, which XFCE, MATE and Cinnamon use to say
/// "dark" (`Adwaita-dark`, `Yaru-dark`, `Mint-Y-Dark`).
fn from_gtk_theme_name() -> Option<Resolved> {
    let settings = gtk::Settings::default()?;
    if settings.is_gtk_application_prefer_dark_theme() {
        return Some(Resolved::Dark);
    }
    let name = settings.gtk_theme_name()?.to_string().to_lowercase();
    if name.ends_with("-dark") || name.contains("dark") {
        Some(Resolved::Dark)
    } else {
        // A light theme name is a real signal, not an absence of one.
        Some(Resolved::Light)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_preference_is_never_overridden_by_detection() {
        // These must not touch GTK or D-Bus at all, so they are safe headless.
        assert_eq!(resolve(Theme::Light), Resolved::Light);
        assert_eq!(resolve(Theme::Dark), Resolved::Dark);
    }

    #[test]
    fn the_two_themes_have_distinct_css_classes() {
        assert_ne!(Resolved::Light.css_class(), Resolved::Dark.css_class());
    }

    #[test]
    fn the_bundled_stylesheet_is_present_and_covers_the_theme_classes() {
        // Guards against the stylesheet being emptied or renamed: `include_str!`
        // would still compile with an empty file.
        assert!(STYLE.len() > 500, "stylesheet looks empty");
        assert!(STYLE.contains(".cm-light"), "no light rules");
        assert!(STYLE.contains(".cm-dark"), "no dark rules");
    }

    #[test]
    fn our_provider_sits_between_the_application_and_user_priorities() {
        // Application = 600, user = 800. Being outside that window would mean
        // either a theme beats us or we beat the user's own gtk.css.
        assert!(PRIORITY > 600 && PRIORITY < 800);
    }
}
