//! Desktop integration written at runtime (§41, §42).
//!
//! The `.deb` installs the system-wide pieces — the binary, the `.desktop`
//! entry in `/usr/share/applications`, icons, the man page. What *cannot* go in
//! a package is anything under `$HOME`, because a package is installed once as
//! root and these files belong to each user who runs the application. So
//! `clipboard-manager install` writes them, and the daemon calls
//! [`sync_autostart`] on every start to keep them matching the current setting.
//!
//! Three files, all under the user's own directories:
//!
//! | Path | Purpose |
//! |---|---|
//! | `~/.config/systemd/user/clipboard-manager.service` | the service unit (§22) |
//! | `~/.config/autostart/clipboard-manager.desktop` | for desktops without systemd user sessions |
//! | `~/.local/share/applications/clipboard-manager.desktop` | the application menu entry |
//!
//! Both autostart mechanisms are written on purpose and do not conflict: the
//! `.desktop` file carries `X-GNOME-Autostart-enabled` and is skipped by
//! desktops that defer to systemd, and the unit's `ExecStart` runs the same
//! single-instance binary, so whichever fires second exits immediately after
//! raising the first (§25).
//!
//! # Never root
//!
//! §28: the unit has no `User=`, is installed under `~/.config/systemd/user`,
//! and is started by `systemctl --user`. There is no system-level unit and no
//! path here resolves outside `$HOME`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::paths;

const SERVICE_NAME: &str = "clipboard-manager.service";
const DESKTOP_NAME: &str = "clipboard-manager.desktop";

/// Absolute path to the running binary, so the unit does not depend on `PATH`.
fn binary_path() -> String {
    std::fs::read_link("/proc/self/exe")
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "/usr/bin/clipboard-manager".to_string())
}

fn systemd_user_dir() -> PathBuf {
    paths::xdg_config_home().join("systemd/user")
}

fn autostart_dir() -> PathBuf {
    paths::xdg_config_home().join("autostart")
}

fn applications_dir() -> PathBuf {
    paths::xdg_data_home().join("applications")
}

/// The systemd user unit (§22, §49).
fn service_unit() -> String {
    format!(
        "\
[Unit]
Description=Clipboard history for Linux desktops
Documentation=man:clipboard-manager(1)
# The graphical session target, so we start once the display server exists.
# Without this the daemon can race the compositor and find no DISPLAY.
PartOf=graphical-session.target
After=graphical-session.target

[Service]
Type=simple
ExecStart={binary} --quiet
ExecReload=/bin/kill -HUP $MAINPID
# Restart on crash so history keeps being recorded (§49), but not when the
# daemon exits cleanly on `clipboard-manager quit` — otherwise quitting would
# be impossible.
Restart=on-failure
RestartSec=2
# Give up after repeated rapid failures rather than looping forever; the
# journal then shows why.
StartLimitIntervalSec=60
StartLimitBurst=5

# Hardening. This is a per-user service that touches only its own data
# directory, the display server socket and the session bus.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=read-only
# ...except our own state, which we must be able to write.
ReadWritePaths=%h/.local/share/clipboard-manager %h/.config/clipboard-manager
RestrictNamespaces=true
RestrictRealtime=true
LockPersonality=true
MemoryDenyWriteExecute=true
# No network: the application is entirely offline (§15).
PrivateNetwork=true
IPAddressDeny=any

[Install]
WantedBy=graphical-session.target
",
        binary = binary_path()
    )
}

/// The `.desktop` entry, used both for the application menu and autostart.
///
/// `autostart` toggles the GNOME-specific key and adds the flag that stops the
/// panel appearing at login.
fn desktop_entry(autostart: bool) -> String {
    let binary = binary_path();
    let mut entry = format!(
        "\
[Desktop Entry]
Type=Application
Name=Clipboard Manager
GenericName=Clipboard History
Comment=Keep a searchable history of what you copy
Exec={binary} %U
TryExec={binary}
Icon=edit-paste
Terminal=false
Categories=Utility;
Keywords=clipboard;history;copy;paste;
StartupNotify=false
# A single instance per session; a second launch raises the first (§25).
SingleMainWindow=true
"
    );

    if autostart {
        entry.push_str(
            "X-GNOME-Autostart-enabled=true\n\
             # Desktops that run a systemd user session start the unit instead,\n\
             # and skip this file.\n\
             X-GNOME-Autostart-Phase=Applications\n",
        );
    }
    entry
}

/// Write every integration file and enable the service.
pub fn install() -> Result<()> {
    let service = write_service()?;
    let menu = write_menu_entry()?;
    let autostart = write_autostart()?;

    println!("Installed:");
    println!("  {}", service.display());
    println!("  {}", menu.display());
    println!("  {}", autostart.display());

    match enable_service() {
        Ok(true) => println!("\nStarted the background service. It will start automatically at login."),
        Ok(false) => println!(
            "\nsystemd is not managing this session, so the autostart entry will \
             start it at login instead."
        ),
        Err(err) => {
            // Not fatal: the autostart entry covers the same ground.
            println!("\nCould not enable the systemd service ({err}).");
            println!("The autostart entry will still start it at login.");
        }
    }

    println!("\nThe shortcut to open the history is {}.", crate::config::DEFAULT_SHORTCUT);
    println!("Run `clipboard-manager status` to check it registered.");
    Ok(())
}

/// Remove everything [`install`] wrote.
pub fn uninstall() -> Result<()> {
    // Stop first, or systemd restarts it while we are deleting the unit.
    let _ = systemctl(&["--user", "stop", SERVICE_NAME]);
    let _ = systemctl(&["--user", "disable", SERVICE_NAME]);

    let paths_to_remove = [
        systemd_user_dir().join(SERVICE_NAME),
        autostart_dir().join(DESKTOP_NAME),
        applications_dir().join(DESKTOP_NAME),
    ];

    for path in &paths_to_remove {
        match std::fs::remove_file(path) {
            Ok(()) => println!("Removed {}", path.display()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => println!("Could not remove {}: {err}", path.display()),
        }
    }

    let _ = systemctl(&["--user", "daemon-reload"]);

    // The shortcut binding lives in the desktop's own configuration.
    let session = crate::platform::Session::detect();
    crate::shortcuts::unregister(&session);

    println!(
        "\nYour clipboard history has been left in place at {}.",
        paths::default_data_dir().display()
    );
    println!("Delete that directory to remove it as well.");
    Ok(())
}

fn write_service() -> Result<PathBuf> {
    let dir = systemd_user_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    let path = dir.join(SERVICE_NAME);
    write_text(&path, &service_unit())?;
    Ok(path)
}

fn write_menu_entry() -> Result<PathBuf> {
    let dir = applications_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    let path = dir.join(DESKTOP_NAME);
    write_text(&path, &desktop_entry(false))?;
    Ok(path)
}

fn write_autostart() -> Result<PathBuf> {
    let dir = autostart_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    let path = dir.join(DESKTOP_NAME);
    write_text(&path, &desktop_entry(true))?;
    Ok(path)
}

/// Add or remove the autostart entry to match the setting.
///
/// Called by the daemon on every start, so toggling "Start on login" in
/// Settings takes effect without the user running anything.
pub fn sync_autostart(enabled: bool) {
    let path = autostart_dir().join(DESKTOP_NAME);
    if enabled {
        if let Err(err) = write_autostart() {
            tracing::warn!(error = %err, "could not write the autostart entry");
        }
    } else if path.exists() {
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!(error = %err, "could not remove the autostart entry");
        }
    }
}

/// `0644`, not `0600`: these are launcher metadata that the desktop shell reads,
/// and they contain no clipboard data. The history itself is `0700`/`0600`,
/// which [`paths`] enforces separately (§29).
fn write_text(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents)
        .with_context(|| format!("could not write {}", path.display()))?;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("could not set permissions on {}", path.display()))?;
    Ok(())
}

/// Enable and start the unit. `Ok(false)` means no systemd user session, which
/// is a supported configuration rather than a failure.
fn enable_service() -> Result<bool> {
    if !has_systemd_user_session() {
        return Ok(false);
    }
    systemctl(&["--user", "daemon-reload"])?;
    systemctl(&["--user", "enable", SERVICE_NAME])?;
    systemctl(&["--user", "restart", SERVICE_NAME])?;
    Ok(true)
}

fn has_systemd_user_session() -> bool {
    // The private bus socket exists exactly when a user instance is running.
    paths::runtime_dir().join("systemd/private").exists()
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("could not run systemctl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "systemctl {} failed: {}",
            args.join(" "),
            if stderr.is_empty() { "no error message".to_string() } else { stderr }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_never_runs_as_root_and_never_touches_the_network() {
        let unit = service_unit();
        // §28: no `User=` directive at all; it is a user unit.
        assert!(!unit.contains("User="), "a user unit must not set User=");
        // §15: entirely offline.
        assert!(unit.contains("PrivateNetwork=true"));
        assert!(unit.contains("IPAddressDeny=any"));
        assert!(unit.contains("NoNewPrivileges=true"));
    }

    #[test]
    fn the_unit_restarts_on_crash_but_not_on_a_clean_quit() {
        let unit = service_unit();
        // §49 wants restart-on-crash; `Restart=always` would make
        // `clipboard-manager quit` impossible.
        assert!(unit.contains("Restart=on-failure"), "{unit}");
        assert!(!unit.contains("Restart=always"), "{unit}");
        // And a burst limit, so a persistent failure does not spin forever.
        assert!(unit.contains("StartLimitBurst="));
    }

    #[test]
    fn the_unit_can_write_its_own_data_despite_protect_home() {
        // ProtectSystem/ProtectHome without ReadWritePaths would make the
        // database unwritable, which would be a silent total failure.
        let unit = service_unit();
        assert!(unit.contains("ProtectHome=read-only"));
        assert!(unit.contains("ReadWritePaths="));
        assert!(unit.contains(".local/share/clipboard-manager"));
        assert!(unit.contains(".config/clipboard-manager"));
    }

    #[test]
    fn the_unit_waits_for_a_graphical_session() {
        // Starting before the compositor means no DISPLAY and no clipboard.
        let unit = service_unit();
        assert!(unit.contains("After=graphical-session.target"));
        assert!(unit.contains("WantedBy=graphical-session.target"));
    }

    #[test]
    fn the_desktop_entry_is_valid_and_names_the_binary_absolutely() {
        let entry = desktop_entry(false);
        assert!(entry.starts_with("[Desktop Entry]"));
        assert!(entry.contains("Type=Application"));
        assert!(entry.contains("Name=Clipboard Manager"));
        assert!(entry.contains("Icon=edit-paste"));
        assert!(entry.contains("Terminal=false"));
        // Every line after the header must be a key=value pair or a comment.
        for line in entry.lines().skip(1).filter(|l| !l.trim().is_empty()) {
            assert!(
                line.starts_with('#') || line.contains('='),
                "not a valid desktop-entry line: {line:?}"
            );
        }
    }

    #[test]
    fn only_the_autostart_copy_carries_the_autostart_keys() {
        assert!(desktop_entry(true).contains("X-GNOME-Autostart-enabled=true"));
        // The menu entry must not autostart, or it would double up.
        assert!(!desktop_entry(false).contains("X-GNOME-Autostart-enabled"));
    }

    #[test]
    fn every_written_path_stays_inside_the_users_own_directories() {
        // §28: nothing here may write outside $HOME.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home".to_string());
        for dir in [systemd_user_dir(), autostart_dir(), applications_dir()] {
            assert!(
                dir.starts_with(&home) || dir.starts_with("/home") || dir.starts_with("/root"),
                "{} escapes the home directory",
                dir.display()
            );
        }
    }
}
