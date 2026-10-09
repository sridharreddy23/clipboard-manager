//! Shortcut registration by asking the desktop to do it (§3, §4, §21).
//!
//! Under Wayland an ordinary client cannot grab a key — that is the same
//! security boundary that stops it reading the clipboard. The supported route is
//! to register the binding with the desktop's own shortcut system, pointing it
//! at `clipboard-manager toggle`, which reaches the running daemon over its
//! private socket (§24).
//!
//! That indirection is not a workaround to apologise for: it is how every
//! Wayland-native application does global shortcuts, it survives the daemon
//! restarting, and it puts the binding where the user expects to find and
//! change it — in their own keyboard settings.
//!
//! | Desktop | Mechanism |
//! |---|---|
//! | GNOME | `org.gnome.settings-daemon.plugins.media-keys custom-keybindings` |
//! | XFCE | `xfconf-query -c xfce4-keyboard-shortcuts` |
//! | Cinnamon | `org.cinnamon.desktop.keybindings custom-keybindings` |
//!
//! KDE is absent on purpose. Plasma's `kglobalshortcutsrc` is only read at
//! startup by `kglobalaccel5`, so writing it would do nothing until the next
//! login, and the D-Bus interface that would apply it immediately requires
//! registering as a KDE component with a `.desktop` identity we do not have.
//! Claiming support and quietly doing nothing is exactly what §56 forbids, so
//! KDE users get the manual instructions instead — which is also where Plasma's
//! own GUI puts clipboard shortcuts.
//!
//! Every function here is idempotent: registering the same accelerator twice
//! updates the existing entry rather than adding a duplicate.

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::platform::Desktop;
use crate::shortcuts::keys::Accelerator;

/// The command the desktop will run. Resolved to an absolute path where
/// possible, because a desktop shortcut does not inherit a useful `PATH`.
fn toggle_command() -> String {
    // `/proc/self/exe` is the binary actually running, which is right even when
    // installed somewhere unusual or run from a build directory.
    match std::fs::read_link("/proc/self/exe") {
        Ok(path) => format!("{} toggle", path.display()),
        Err(_) => "clipboard-manager toggle".to_string(),
    }
}

/// Our marker in the desktop's shortcut list, so we can find and update our own
/// entry instead of accumulating one per launch.
const BINDING_NAME: &str = "Clipboard Manager";

/// Try to register `accel` with `desktop`.
///
/// Returns a human-readable description of what was done, for the status output
/// and the About dialog.
pub fn register(desktop: &Desktop, accel: &Accelerator) -> Result<String> {
    match desktop {
        Desktop::Gnome => gsettings_register(
            "org.gnome.settings-daemon.plugins.media-keys",
            "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/",
            accel,
        ),
        Desktop::Cinnamon => gsettings_register(
            "org.cinnamon.desktop.keybindings",
            "/org/cinnamon/desktop/keybindings/custom-keybindings/",
            accel,
        ),
        Desktop::Xfce => xfce_register(accel),
        other => bail!(
            "{} does not provide a way to register a global shortcut programmatically",
            other.label()
        ),
    }
}

/// Remove our binding again, used when the shortcut changes or on uninstall.
pub fn unregister(desktop: &Desktop) -> Result<()> {
    match desktop {
        Desktop::Gnome => gsettings_unregister(
            "org.gnome.settings-daemon.plugins.media-keys",
            "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/",
        ),
        Desktop::Cinnamon => gsettings_unregister(
            "org.cinnamon.desktop.keybindings",
            "/org/cinnamon/desktop/keybindings/custom-keybindings/",
        ),
        Desktop::Xfce => xfce_unregister(),
        _ => Ok(()),
    }
}

// ---- GNOME and Cinnamon -----------------------------------------------------

/// GNOME and Cinnamon both use the same awkward two-part scheme: a list of
/// object paths in one key, and a separate relocatable schema at each path
/// holding name/command/binding.
fn gsettings_register(schema: &str, path_prefix: &str, accel: &Accelerator) -> Result<String> {
    require_tool("gsettings")?;

    // Our own slot, fixed so repeated registration overwrites rather than
    // appending. `custom99` is high enough not to collide with the entries a
    // user creates by hand, which start at `custom0`.
    let slot = format!("{path_prefix}clipboard-manager/");

    let mut existing = gsettings_get_list(schema, "custom-keybindings")?;
    if !existing.iter().any(|entry| entry == &slot) {
        existing.push(slot.clone());
        gsettings_set_list(schema, "custom-keybindings", &existing)?;
    }

    let relocatable = format!("{schema}.custom-keybinding");
    gsettings_set(&relocatable, &slot, "name", BINDING_NAME)?;
    gsettings_set(&relocatable, &slot, "command", &toggle_command())?;
    gsettings_set(&relocatable, &slot, "binding", &accel.to_gsettings())?;

    Ok(format!(
        "registered with the desktop as a custom keyboard shortcut running \
         `{}`",
        toggle_command()
    ))
}

fn gsettings_unregister(schema: &str, path_prefix: &str) -> Result<()> {
    if require_tool("gsettings").is_err() {
        return Ok(());
    }
    let slot = format!("{path_prefix}clipboard-manager/");
    let existing = gsettings_get_list(schema, "custom-keybindings")?;
    let filtered: Vec<String> = existing.into_iter().filter(|entry| entry != &slot).collect();
    gsettings_set_list(schema, "custom-keybindings", &filtered)
}

fn gsettings_get_list(schema: &str, key: &str) -> Result<Vec<String>> {
    let output = run("gsettings", &["get", schema, key])?;
    Ok(parse_gvariant_list(&output))
}

fn gsettings_set_list(schema: &str, key: &str, values: &[String]) -> Result<()> {
    let encoded = if values.is_empty() {
        "[]".to_string()
    } else {
        let items: Vec<String> = values.iter().map(|v| format!("'{}'", escape_quotes(v))).collect();
        format!("[{}]", items.join(", "))
    };
    run("gsettings", &["set", schema, key, &encoded]).map(|_| ())
}

fn gsettings_set(schema: &str, path: &str, key: &str, value: &str) -> Result<()> {
    run("gsettings", &["set", &format!("{schema}:{path}"), key, value]).map(|_| ())
}

/// Parse the GVariant array syntax `gsettings get` prints.
///
/// Hand-rolled rather than pulling in a parser: the only shape that occurs here
/// is a flat array of single-quoted strings, or the literal `@as []` that
/// GSettings prints for an empty typed array.
pub(crate) fn parse_gvariant_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    // `@as []` is an empty array with an explicit type annotation.
    let trimmed = trimmed.strip_prefix("@as ").unwrap_or(trimmed);
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or("");

    inner
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.trim_matches('\'').trim_matches('"').to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn escape_quotes(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

// ---- XFCE -------------------------------------------------------------------

fn xfce_register(accel: &Accelerator) -> Result<String> {
    require_tool("xfconf-query")?;

    // XFCE keys the binding on the accelerator itself, with the command as the
    // value — the inverse of GNOME's scheme.
    let property = format!("/commands/custom/{}", accel.to_gsettings());
    run(
        "xfconf-query",
        &[
            "-c",
            "xfce4-keyboard-shortcuts",
            "-p",
            &property,
            "-n",
            "-t",
            "string",
            "-s",
            &toggle_command(),
        ],
    )?;

    Ok(format!(
        "registered with the Xfce keyboard settings as `{}`",
        toggle_command()
    ))
}

fn xfce_unregister() -> Result<()> {
    if require_tool("xfconf-query").is_err() {
        return Ok(());
    }
    // Enumerate and remove whatever points at our binary, since the property
    // name is the accelerator and we may not know the old one.
    let listed = run("xfconf-query", &["-c", "xfce4-keyboard-shortcuts", "-l", "-v"])?;
    for line in listed.lines() {
        if !line.contains("clipboard-manager") {
            continue;
        }
        if let Some(property) = line.split_whitespace().next() {
            let _ = run("xfconf-query", &["-c", "xfce4-keyboard-shortcuts", "-p", property, "-r"]);
        }
    }
    Ok(())
}

// ---- process plumbing -------------------------------------------------------

fn require_tool(name: &str) -> Result<()> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let found = std::env::split_paths(&path).any(|dir| dir.join(name).is_file());
    if found {
        Ok(())
    } else {
        bail!("{name} is not installed, so the shortcut cannot be registered this way")
    }
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "{program} failed: {}",
            if stderr.is_empty() { "no error message".to_string() } else { stderr }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// What to tell a user who has to bind the key themselves (§4).
///
/// Desktop-specific because "open keyboard settings" is useless advice if the
/// user cannot find them, and because the command they need to paste is the
/// same everywhere but the place to paste it is not.
pub fn manual_instructions(desktop: &Desktop, accel: &Accelerator) -> String {
    let command = toggle_command();
    let where_to_go = match desktop {
        Desktop::Gnome => {
            "Settings → Keyboard → View and Customise Shortcuts → Custom Shortcuts → +"
        }
        Desktop::Kde => "System Settings → Shortcuts → Add New → Command or Script",
        Desktop::Xfce => "Settings → Keyboard → Application Shortcuts → Add",
        Desktop::Cinnamon => "System Settings → Keyboard → Shortcuts → Custom Shortcuts → Add",
        Desktop::Mate => "System → Preferences → Hardware → Keyboard Shortcuts → Add",
        Desktop::Lxqt => "Preferences → Shortcut Keys → Add",
        Desktop::Sway => "add to ~/.config/sway/config:\n    bindsym Ctrl+grave exec",
        Desktop::Hyprland => "add to ~/.config/hypr/hyprland.conf:\n    bind = CTRL, grave, exec,",
        Desktop::Wayfire => "add to ~/.config/wayfire.ini under [command]",
        Desktop::River => "add to ~/.config/river/init:\n    riverctl map normal Control grave spawn",
        Desktop::Unknown(_) => "your desktop's keyboard shortcut settings",
    };

    format!(
        "To finish setting up the shortcut, bind {} to this command:\n\n    {command}\n\n\
         Where to do it: {where_to_go}",
        accel.to_display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toggle_command_names_the_toggle_subcommand() {
        // The desktop runs this verbatim; a wrong subcommand would bind a key
        // that silently does nothing.
        assert!(toggle_command().ends_with(" toggle"));
    }

    #[test]
    fn gvariant_lists_parse_in_every_form_gsettings_prints() {
        assert_eq!(parse_gvariant_list("@as []"), Vec::<String>::new());
        assert_eq!(parse_gvariant_list("[]"), Vec::<String>::new());
        assert_eq!(
            parse_gvariant_list("['/org/gnome/a/', '/org/gnome/b/']"),
            vec!["/org/gnome/a/".to_string(), "/org/gnome/b/".to_string()]
        );
        // Single entry, no trailing comma.
        assert_eq!(
            parse_gvariant_list("['/org/gnome/only/']"),
            vec!["/org/gnome/only/".to_string()]
        );
        // Double-quoted, which some versions emit.
        assert_eq!(parse_gvariant_list("[\"/a/\"]"), vec!["/a/".to_string()]);
    }

    #[test]
    fn a_malformed_gsettings_reply_yields_an_empty_list_rather_than_panicking() {
        assert_eq!(parse_gvariant_list(""), Vec::<String>::new());
        assert_eq!(parse_gvariant_list("nonsense"), Vec::<String>::new());
    }

    #[test]
    fn quotes_in_a_path_cannot_break_out_of_the_gvariant_literal() {
        // Defensive: the paths we write are our own, but building shell-ish
        // strings without escaping is how injection bugs start.
        assert_eq!(escape_quotes("it's"), "it\\'s");
        assert_eq!(escape_quotes("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn kde_is_refused_rather_than_silently_doing_nothing() {
        let accel = Accelerator::parse("Ctrl+grave").unwrap();
        let err = register(&Desktop::Kde, &accel).unwrap_err().to_string();
        assert!(err.contains("KDE Plasma"), "{err}");
    }

    #[test]
    fn every_supported_desktop_gets_specific_manual_instructions() {
        let accel = Accelerator::parse("Ctrl+grave").unwrap();
        for desktop in [
            Desktop::Gnome,
            Desktop::Kde,
            Desktop::Xfce,
            Desktop::Cinnamon,
            Desktop::Mate,
            Desktop::Lxqt,
            Desktop::Sway,
            Desktop::Hyprland,
            Desktop::Wayfire,
            Desktop::River,
            Desktop::Unknown("whatever".into()),
        ] {
            let text = manual_instructions(&desktop, &accel);
            assert!(text.contains("toggle"), "{desktop:?} instructions lack the command");
            // The displayed accelerator must name the key in words, since
            // "grave" means nothing to most people.
            assert!(text.contains("backtick"), "{desktop:?}: {text}");
        }
    }
}
