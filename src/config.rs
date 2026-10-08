//! Persistent user settings (§29).
//!
//! Settings live in a single TOML file at
//! `$XDG_CONFIG_HOME/clipboard-manager/config.toml`, written `0600`.
//!
//! Every field has a `#[serde(default)]`-backed default, so a config file
//! written by an older version — or a hand-edited file with a typo in one
//! section — still loads. A file that cannot be parsed at all is preserved as
//! `config.toml.broken` and replaced with defaults rather than taking the
//! daemon down (§35).

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// The shortcut shipped as the default. This is the single source of truth for
/// it; the CLI help, the settings UI and the documentation all read it from
/// here.
pub const DEFAULT_SHORTCUT: &str = "Ctrl+grave";

/// Theme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the desktop's light/dark preference.
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }
}

/// What happens when the user activates an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActivateAction {
    /// Copy to the clipboard and close the popup. The user then pastes with
    /// their own paste shortcut. This is the safe default mandated by §51.
    #[default]
    CopyAndClose,
    /// Copy to the clipboard and leave the popup open, for collecting several
    /// items in a row.
    CopyAndStayOpen,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Install/maintain the autostart entry (§42).
    pub start_on_login: bool,
    /// Show a `StatusNotifierItem` tray icon where the desktop supports one.
    pub show_tray_icon: bool,
    /// Global shortcut, in the format understood by [`crate::shortcuts::keys`].
    pub shortcut: String,
    /// When true, starting the application does not open any window.
    pub launch_minimized: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            start_on_login: true,
            show_tray_icon: true,
            shortcut: DEFAULT_SHORTCUT.to_string(),
            launch_minimized: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClipboardSettings {
    /// Maximum number of unpinned entries to keep. `0` means unlimited.
    pub max_history: u32,
    /// Delete unpinned entries older than this many days. `0` means never.
    pub retention_days: u32,
    pub store_images: bool,
    pub store_files: bool,
    /// Collapse a repeated copy onto the existing entry instead of adding a
    /// new one (§10).
    pub ignore_duplicates: bool,
    /// Also record the X11 PRIMARY selection. Off by default: PRIMARY changes
    /// on every drag-select and would flood the history (§19).
    pub monitor_primary: bool,
    /// Reject clippings larger than this many megabytes. `0` means unlimited.
    pub max_item_size_mb: u64,
    /// Trim trailing whitespace/newlines from text clippings before storing.
    pub trim_whitespace: bool,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            max_history: 250,
            retention_days: 30,
            store_images: true,
            store_files: true,
            ignore_duplicates: true,
            monitor_primary: false,
            max_item_size_mb: 50,
            trim_whitespace: false,
        }
    }
}

impl ClipboardSettings {
    /// Size limit in bytes, or `None` when unlimited.
    pub fn max_item_bytes(&self) -> Option<u64> {
        if self.max_item_size_mb == 0 {
            None
        } else {
            Some(self.max_item_size_mb * 1024 * 1024)
        }
    }

    /// Offered history-size choices for the settings UI (§14).
    pub const HISTORY_CHOICES: [(u32, &'static str); 6] = [
        (50, "50 items"),
        (100, "100 items"),
        (250, "250 items"),
        (500, "500 items"),
        (1000, "1000 items"),
        (0, "Unlimited"),
    ];

    /// Offered retention choices (§14).
    pub const RETENTION_CHOICES: [(u32, &'static str); 5] = [
        (0, "Never"),
        (1, "1 day"),
        (7, "7 days"),
        (30, "30 days"),
        (90, "90 days"),
    ];

    /// Offered per-item size limits (§33).
    pub const SIZE_CHOICES: [(u64, &'static str); 4] =
        [(10, "10 MB"), (50, "50 MB"), (100, "100 MB"), (0, "Unlimited")];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Privacy {
    /// Monitoring is suspended. History remains readable.
    pub paused: bool,
    /// Wipe all unpinned history when the daemon exits cleanly.
    pub clear_on_exit: bool,
    /// Also wipe pinned entries on exit. Only meaningful with
    /// `clear_on_exit`; separate so that the common case cannot destroy pins
    /// by accident.
    pub clear_pinned_on_exit: bool,
    /// Names matched against the detected source application (X11 only).
    /// Matching is case-insensitive substring matching against the process
    /// name, the executable path and `WM_CLASS`.
    pub excluded_apps: Vec<String>,
    /// Honour password-manager hints from the source application
    /// (`x-kde-passwordManagerHint`). Exact, so it is on by default.
    pub skip_passwords: bool,
    /// Additionally drop text that *looks* like a generated credential.
    ///
    /// Off by default: unlike the hint above this is a guess, and silently
    /// discarding an API key the user meant to keep would be worse than keeping
    /// one in a `0600` database on their own machine. See
    /// [`crate::privacy::looks_like_credential`].
    pub skip_secret_like_text: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Self {
            paused: false,
            clear_on_exit: false,
            clear_pinned_on_exit: false,
            // Pre-seeded with the common password managers so the feature is
            // useful before the user ever opens Settings. These are matched as
            // substrings, so e.g. `keepassxc` also matches `/usr/bin/keepassxc`.
            excluded_apps: vec![
                "keepassxc".to_string(),
                "bitwarden".to_string(),
                "1password".to_string(),
                "gnome-keyring".to_string(),
                "seahorse".to_string(),
            ],
            skip_passwords: true,
            skip_secret_like_text: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    pub theme: Theme,
    /// Popup width in logical pixels.
    pub popup_width: i32,
    /// Popup height in logical pixels.
    pub popup_height: i32,
    /// Characters of text kept for the list preview.
    pub preview_length: u32,
    pub show_timestamps: bool,
    /// Thumbnail edge length for image rows.
    pub thumbnail_size: i32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            // Windows' clipboard flyout is a narrow vertical panel; these
            // proportions mirror it.
            popup_width: 380,
            popup_height: 540,
            preview_length: 220,
            show_timestamps: true,
            thumbnail_size: 64,
        }
    }
}

impl Appearance {
    pub const WIDTH_RANGE: (i32, i32) = (300, 720);
    pub const HEIGHT_RANGE: (i32, i32) = (320, 1000);
    pub const PREVIEW_RANGE: (u32, u32) = (40, 1000);
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Advanced {
    /// Override for the data directory. `None` uses the XDG default.
    pub storage_path: Option<PathBuf>,
    /// Raise the log level to `debug` without needing `--debug`.
    pub debug_logging: bool,
    /// What activating an entry does.
    pub activate_action: ActivateAction,
}

// Every field's own default is already what we want: no storage override,
// quiet logging, and the safe copy-and-close activation (§51).

/// The complete settings document.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub general: General,
    pub clipboard: ClipboardSettings,
    pub privacy: Privacy,
    pub appearance: Appearance,
    pub advanced: Advanced,
}

impl Settings {
    /// Load from the default location, falling back to defaults on any error.
    ///
    /// Never fails: a clipboard daemon that refuses to start because of a
    /// malformed config file is worse than one that starts with defaults and
    /// says so.
    pub fn load() -> Self {
        let path = paths::config_file();
        match Self::load_from(&path) {
            Ok(settings) => settings,
            Err(err) => {
                if path.exists() {
                    let broken = path.with_extension("toml.broken");
                    tracing::error!(
                        path = %path.display(),
                        backup = %broken.display(),
                        error = %err,
                        "configuration file is unreadable; continuing with defaults"
                    );
                    let _ = std::fs::rename(&path, &broken);
                } else {
                    tracing::debug!("no configuration file yet; using defaults");
                }
                Self::default()
            }
        }
    }

    /// Load from an explicit path. Missing file yields defaults.
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).context("failed to read configuration"),
        };
        let settings: Settings = toml::from_str(&text).context("failed to parse configuration")?;
        Ok(settings.normalized())
    }

    /// Persist to the default location.
    pub fn save(&self) -> Result<()> {
        self.save_to(&paths::config_file())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("failed to serialise configuration")?;
        let header = "# clipboard-manager configuration\n\
                      # This file is rewritten by the Settings window; comments are not preserved.\n\
                      # Documentation: /usr/share/doc/clipboard-manager/README.md\n\n";
        paths::write_private_atomic(path, format!("{header}{text}").as_bytes())
    }

    /// Clamp out-of-range values that a hand-edited file could contain.
    ///
    /// Done on load rather than on use so that every consumer can trust the
    /// values without re-checking them.
    pub fn normalized(mut self) -> Self {
        let (wmin, wmax) = Appearance::WIDTH_RANGE;
        let (hmin, hmax) = Appearance::HEIGHT_RANGE;
        let (pmin, pmax) = Appearance::PREVIEW_RANGE;
        self.appearance.popup_width = self.appearance.popup_width.clamp(wmin, wmax);
        self.appearance.popup_height = self.appearance.popup_height.clamp(hmin, hmax);
        self.appearance.preview_length = self.appearance.preview_length.clamp(pmin, pmax);
        self.appearance.thumbnail_size = self.appearance.thumbnail_size.clamp(32, 160);
        if self.general.shortcut.trim().is_empty() {
            self.general.shortcut = DEFAULT_SHORTCUT.to_string();
        }
        self.privacy.excluded_apps = self
            .privacy
            .excluded_apps
            .into_iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        self.privacy.excluded_apps.sort();
        self.privacy.excluded_apps.dedup();
        self
    }

    /// Resolved data directory.
    pub fn data_dir(&self) -> PathBuf {
        self.advanced
            .storage_path
            .clone()
            .unwrap_or_else(paths::default_data_dir)
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir().join("database.sqlite")
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.data_dir().join("blobs")
    }
}

impl fmt::Display for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match toml::to_string_pretty(self) {
            Ok(s) => f.write_str(&s),
            Err(_) => f.write_str("<unserialisable settings>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let settings = Settings::default();
        let text = toml::to_string_pretty(&settings).unwrap();
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(settings, parsed);
    }

    #[test]
    fn default_shortcut_is_ctrl_backtick() {
        assert_eq!(Settings::default().general.shortcut, "Ctrl+grave");
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        // A file that only sets one key in one section must not reset the rest.
        let text = r#"
            [appearance]
            theme = "dark"
        "#;
        let parsed: Settings = toml::from_str(text).unwrap();
        assert_eq!(parsed.appearance.theme, Theme::Dark);
        assert_eq!(parsed.clipboard.max_history, 250);
        assert_eq!(parsed.general.shortcut, "Ctrl+grave");
    }

    #[test]
    fn normalization_clamps_and_cleans() {
        let mut s = Settings::default();
        s.appearance.popup_width = 10_000;
        s.appearance.preview_length = 0;
        s.general.shortcut = "   ".to_string();
        s.privacy.excluded_apps = vec![
            "  KeePassXC ".into(),
            "keepassxc".into(),
            "".into(),
            "Bitwarden".into(),
        ];
        let s = s.normalized();

        assert_eq!(s.appearance.popup_width, Appearance::WIDTH_RANGE.1);
        assert_eq!(s.appearance.preview_length, Appearance::PREVIEW_RANGE.0);
        assert_eq!(s.general.shortcut, DEFAULT_SHORTCUT);
        assert_eq!(s.privacy.excluded_apps, vec!["bitwarden", "keepassxc"]);
    }

    #[test]
    fn size_limit_zero_means_unlimited() {
        let mut c = ClipboardSettings::default();
        assert_eq!(c.max_item_bytes(), Some(50 * 1024 * 1024));
        c.max_item_size_mb = 0;
        assert_eq!(c.max_item_bytes(), None);
    }

    #[test]
    fn save_then_load_is_identity_and_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        let mut settings = Settings::default();
        settings.appearance.theme = Theme::Dark;
        settings.clipboard.max_history = 0;
        settings.privacy.excluded_apps = vec!["bitwarden".into()];
        settings.save_to(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config file must not be readable by other users");

        assert_eq!(Settings::load_from(&path).unwrap(), settings);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = Settings::load_from(&tmp.path().join("absent.toml")).unwrap();
        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn unparseable_file_is_an_error_at_this_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();
        assert!(Settings::load_from(&path).is_err());
    }
}
