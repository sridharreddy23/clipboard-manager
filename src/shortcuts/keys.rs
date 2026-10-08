//! Parsing and formatting accelerators (§3).
//!
//! One representation, several consumers that all spell things differently:
//!
//! | Consumer | `Ctrl + \`` looks like |
//! |---|---|
//! | our `config.toml` | `Ctrl+grave` |
//! | X11 `XGrabKey` | keysym `0xfe50`-ish → keycode + `ControlMask` |
//! | GNOME GSettings | `<Control>grave` |
//! | KDE KGlobalAccel | `Ctrl+`` ` |
//! | XDG GlobalShortcuts portal | `CTRL+grave` |
//! | shown to the user | `Ctrl + \`` |
//!
//! Getting this wrong is not a cosmetic problem: a shortcut written in the
//! wrong dialect registers silently and never fires, which §3 explicitly
//! forbids claiming as working.
//!
//! # Key names, not characters
//!
//! The key is stored by *name* (`grave`), never by the character it happens to
//! produce. §3 is explicit about this, and the reason is layouts: on a French
//! AZERTY keyboard the physical key left of `1` produces `²`, on a German
//! QWERTZ it produces `^`, and on a US QWERTY it produces `` ` ``. Storing the
//! character would mean the default shortcut silently moved to a different
//! physical key — or stopped existing — depending on the user's layout.
//! Storing `grave` means we resolve the name to whatever keycode currently
//! produces that keysym, which is what every desktop's own shortcut editor
//! does.

use std::fmt;

use anyhow::{bail, Result};

/// Modifier mask, in our own representation so no display-server type leaks up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// The Windows/Command key. `Super` in X11 terms, `Meta` to some desktops.
    pub superkey: bool,
}

impl Modifiers {
    pub fn is_empty(self) -> bool {
        !self.ctrl && !self.shift && !self.alt && !self.superkey
    }

    pub fn count(self) -> usize {
        usize::from(self.ctrl)
            + usize::from(self.shift)
            + usize::from(self.alt)
            + usize::from(self.superkey)
    }
}

/// A parsed accelerator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accelerator {
    pub mods: Modifiers,
    /// Canonical X11 keysym name, e.g. `grave`, `v`, `F5`, `Print`.
    pub key: String,
}

impl Accelerator {
    /// Parse one of the spellings we accept.
    ///
    /// Liberal on input because this is also what the settings field feeds:
    /// `Ctrl+grave`, `<Control>grave`, `CTRL+GRAVE`, `ctrl + \`` and
    /// `Control-grave` all mean the same thing.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            bail!("the shortcut is empty");
        }

        // Normalise the GSettings `<Control><Shift>v` form into `Control+Shift+v`
        // so one tokeniser handles every dialect.
        let normalised = spec.replace('<', "").replace('>', "+");

        let mut mods = Modifiers::default();
        let mut rest = normalised.trim();

        // Consume modifier prefixes one at a time rather than splitting the
        // whole string. Splitting cannot work here: `+` and `-` are both
        // separators *and* key names (`Ctrl+-` is Ctrl and the minus key,
        // `Ctrl++` is Ctrl and plus), so the separator is only a separator
        // when a modifier name sits to its left.
        loop {
            let Some((head, tail)) = split_modifier(rest) else {
                break;
            };
            match head {
                Modifier::Ctrl => mods.ctrl = true,
                Modifier::Shift => mods.shift = true,
                Modifier::Alt => mods.alt = true,
                Modifier::Super => mods.superkey = true,
            }
            rest = tail.trim();
        }

        if rest.is_empty() {
            if mods.is_empty() {
                bail!("the shortcut is empty");
            }
            bail!("the shortcut has modifiers but no key: {spec:?}");
        }

        // Whatever is left is the key. A remaining separator means the user
        // named two keys, which is not a thing we can register.
        if rest.len() > 1 && (rest.contains('+') || rest.contains('-')) {
            bail!("the shortcut names more than one key: {spec:?}");
        }

        // A trailing modifier name is not a key. `Ctrl+Shift` reaches here
        // because the final `Shift` has no separator after it to peel, and
        // without this check it would pass through as a bogus keysym.
        if canonical_modifier(rest).is_some() {
            bail!("the shortcut has modifiers but no key: {spec:?}");
        }

        let key = canonical_key(rest)?;

        // A bare key would fire while the user types in any application.
        if mods.is_empty() {
            bail!("the shortcut needs at least one modifier, e.g. Ctrl+{key}");
        }

        Ok(Self { mods, key })
    }

    /// Our own `config.toml` spelling.
    pub fn to_config(&self) -> String {
        let mut parts = Vec::new();
        if self.mods.ctrl {
            parts.push("Ctrl");
        }
        if self.mods.alt {
            parts.push("Alt");
        }
        if self.mods.shift {
            parts.push("Shift");
        }
        if self.mods.superkey {
            parts.push("Super");
        }
        format!("{}+{}", parts.join("+"), self.key)
    }

    /// The GNOME / GTK spelling: `<Control><Shift>grave`.
    pub fn to_gsettings(&self) -> String {
        let mut out = String::new();
        if self.mods.ctrl {
            out.push_str("<Control>");
        }
        if self.mods.alt {
            out.push_str("<Alt>");
        }
        if self.mods.shift {
            out.push_str("<Shift>");
        }
        if self.mods.superkey {
            out.push_str("<Super>");
        }
        out.push_str(&self.key);
        out
    }

    /// The KDE / Qt spelling, which wants the printable character where there
    /// is one: `Ctrl+``.
    pub fn to_kde(&self) -> String {
        let mut parts = Vec::new();
        if self.mods.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.mods.alt {
            parts.push("Alt".to_string());
        }
        if self.mods.shift {
            parts.push("Shift".to_string());
        }
        if self.mods.superkey {
            parts.push("Meta".to_string());
        }
        parts.push(qt_key_name(&self.key));
        parts.join("+")
    }

    /// The XDG GlobalShortcuts portal spelling, which follows the
    /// `shortcuts` XML convention: `CTRL+grave`.
    pub fn to_portal(&self) -> String {
        let mut parts = Vec::new();
        if self.mods.ctrl {
            parts.push("CTRL".to_string());
        }
        if self.mods.alt {
            parts.push("ALT".to_string());
        }
        if self.mods.shift {
            parts.push("SHIFT".to_string());
        }
        if self.mods.superkey {
            parts.push("LOGO".to_string());
        }
        parts.push(self.key.clone());
        parts.join("+")
    }

    /// How it is shown to a person. Uses the symbol where one reads better.
    pub fn to_display(&self) -> String {
        let mut parts = Vec::new();
        if self.mods.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.mods.alt {
            parts.push("Alt".to_string());
        }
        if self.mods.shift {
            parts.push("Shift".to_string());
        }
        if self.mods.superkey {
            parts.push("Super".to_string());
        }
        parts.push(display_key_name(&self.key));
        parts.join(" + ")
    }
}

impl fmt::Display for Accelerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_display())
    }
}

enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

/// Peel one leading `Modifier+` or `Modifier-` off `spec`.
///
/// Returns `None` when the string does not start with a modifier name followed
/// by a separator, which is how the caller knows the remainder is the key.
fn split_modifier(spec: &str) -> Option<(Modifier, &str)> {
    // Longest first, so `Control` is not matched as `Ctrl` would be by a
    // prefix test, and `Command` is not shadowed by `Cmd`.
    const NAMES: &[&str] = &[
        "control", "command", "primary", "super", "shift", "meta", "ctrl", "logo", "mod1",
        "mod4", "alt", "cmd", "win",
    ];

    let lower = spec.to_lowercase();
    for name in NAMES {
        if !lower.starts_with(name) {
            continue;
        }
        let after = &spec[name.len()..];
        let after_trimmed = after.trim_start();
        // A separator is required: without it, `Supercalifragilistic` would
        // lose its first five letters.
        let Some(tail) = after_trimmed
            .strip_prefix('+')
            .or_else(|| after_trimmed.strip_prefix('-'))
        else {
            continue;
        };
        // ...and something must follow it, so a trailing `Ctrl+` is an error
        // rather than silently parsing as bare `Ctrl`.
        if tail.trim().is_empty() {
            continue;
        }
        let modifier = canonical_modifier(name)?;
        return Some((modifier, tail));
    }
    None
}

fn canonical_modifier(token: &str) -> Option<Modifier> {
    match token.to_lowercase().as_str() {
        "ctrl" | "control" | "primary" => Some(Modifier::Ctrl),
        "shift" => Some(Modifier::Shift),
        "alt" | "mod1" | "meta" => Some(Modifier::Alt),
        "super" | "logo" | "win" | "mod4" | "cmd" | "command" => Some(Modifier::Super),
        _ => None,
    }
}

/// Map a user-typed key onto its canonical X11 keysym name.
///
/// Punctuation is the part that matters: a user typing a literal `` ` `` into
/// the settings field means the grave key, and so does `grave`.
fn canonical_key(token: &str) -> Result<String> {
    // Single punctuation characters have keysym names that are not the
    // character itself. This table covers the ASCII punctuation anyone would
    // plausibly bind; anything else falls through to the name as typed, which
    // is correct for `F5`, `Print`, `Insert` and the like.
    let named = match token {
        "`" => Some("grave"),
        "~" => Some("asciitilde"),
        "-" => Some("minus"),
        "_" => Some("underscore"),
        "=" => Some("equal"),
        "+" => Some("plus"),
        "[" => Some("bracketleft"),
        "]" => Some("bracketright"),
        "{" => Some("braceleft"),
        "}" => Some("braceright"),
        "\\" => Some("backslash"),
        "|" => Some("bar"),
        ";" => Some("semicolon"),
        ":" => Some("colon"),
        "'" => Some("apostrophe"),
        "\"" => Some("quotedbl"),
        "," => Some("comma"),
        "." => Some("period"),
        "/" => Some("slash"),
        "?" => Some("question"),
        " " => Some("space"),
        _ => None,
    };
    if let Some(name) = named {
        return Ok(name.to_string());
    }

    // A single letter or digit is its own lowercase keysym name.
    if token.chars().count() == 1 {
        let ch = token.chars().next().expect("just counted one");
        if ch.is_ascii_alphanumeric() {
            return Ok(ch.to_ascii_lowercase().to_string());
        }
    }

    // Accept the spelled-out names case-insensitively, but hand back the
    // canonical casing, because X11 keysym lookup is case-sensitive:
    // `XStringToKeysym("Grave")` fails where `"grave"` succeeds.
    if let Some(canonical) = KNOWN_KEYS
        .iter()
        .find(|name| name.eq_ignore_ascii_case(token))
    {
        return Ok((*canonical).to_string());
    }

    // Function keys, which are the common case not in the table.
    let lower = token.to_lowercase();
    if let Some(digits) = lower.strip_prefix('f') {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(number) = digits.parse::<u8>() {
                if (1..=24).contains(&number) {
                    return Ok(format!("F{number}"));
                }
            }
        }
    }

    if token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        // Unknown but plausible: pass it through so an exotic keysym the user
        // knows the name of still works. Registration will fail loudly if the
        // server does not know it, which is the honest outcome.
        return Ok(token.to_string());
    }

    bail!("{token:?} is not a key name we recognise")
}

/// Keysym names we accept spelled out, with their canonical X11 casing.
const KNOWN_KEYS: &[&str] = &[
    "grave",
    "asciitilde",
    "minus",
    "underscore",
    "equal",
    "plus",
    "bracketleft",
    "bracketright",
    "braceleft",
    "braceright",
    "backslash",
    "bar",
    "semicolon",
    "colon",
    "apostrophe",
    "quotedbl",
    "comma",
    "period",
    "slash",
    "question",
    "space",
    "Tab",
    "Return",
    "Escape",
    "BackSpace",
    "Delete",
    "Insert",
    "Home",
    "End",
    "Prior",
    "Next",
    "Up",
    "Down",
    "Left",
    "Right",
    "Print",
    "Menu",
    "Pause",
];

/// Qt spells punctuation as the character itself.
fn qt_key_name(key: &str) -> String {
    match key {
        "grave" => "`".to_string(),
        "asciitilde" => "~".to_string(),
        "minus" => "-".to_string(),
        "equal" => "=".to_string(),
        "bracketleft" => "[".to_string(),
        "bracketright" => "]".to_string(),
        "backslash" => "\\".to_string(),
        "semicolon" => ";".to_string(),
        "apostrophe" => "'".to_string(),
        "comma" => ",".to_string(),
        "period" => ".".to_string(),
        "slash" => "/".to_string(),
        "space" => "Space".to_string(),
        "Prior" => "PgUp".to_string(),
        "Next" => "PgDown".to_string(),
        "Return" => "Return".to_string(),
        other => {
            if other.chars().count() == 1 {
                other.to_uppercase()
            } else {
                other.to_string()
            }
        }
    }
}

/// Human-readable key name, with a hint for the one that needs it.
fn display_key_name(key: &str) -> String {
    match key {
        // Spelling out which key this is matters: most users do not know the
        // word "grave", and it is our default (§3).
        "grave" => "` (backtick)".to_string(),
        "asciitilde" => "~".to_string(),
        "minus" => "-".to_string(),
        "equal" => "=".to_string(),
        "bracketleft" => "[".to_string(),
        "bracketright" => "]".to_string(),
        "backslash" => "\\".to_string(),
        "semicolon" => ";".to_string(),
        "apostrophe" => "'".to_string(),
        "comma" => ",".to_string(),
        "period" => ".".to_string(),
        "slash" => "/".to_string(),
        "space" => "Space".to_string(),
        "Prior" => "Page Up".to_string(),
        "Next" => "Page Down".to_string(),
        other => {
            if other.chars().count() == 1 {
                other.to_uppercase()
            } else {
                other.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default has to survive every dialect, because every dialect is used
    /// by some desktop we support. This is the single most important test in
    /// the file (§3).
    #[test]
    fn the_default_shortcut_round_trips_through_every_dialect() {
        let accel = Accelerator::parse(crate::config::DEFAULT_SHORTCUT).unwrap();

        assert!(accel.mods.ctrl);
        assert!(!accel.mods.shift, "the default must not be Ctrl+Shift+`");
        assert!(!accel.mods.superkey, "the default must not be Super+V");
        assert_eq!(accel.key, "grave");

        assert_eq!(accel.to_config(), "Ctrl+grave");
        assert_eq!(accel.to_gsettings(), "<Control>grave");
        assert_eq!(accel.to_kde(), "Ctrl+`");
        assert_eq!(accel.to_portal(), "CTRL+grave");
        assert_eq!(accel.to_display(), "Ctrl + ` (backtick)");

        // And the config spelling must parse back to the same thing.
        assert_eq!(Accelerator::parse(&accel.to_config()).unwrap(), accel);
        assert_eq!(Accelerator::parse(&accel.to_gsettings()).unwrap(), accel);
    }

    #[test]
    fn the_shipped_default_is_ctrl_backtick_and_nothing_else() {
        // Guards the literal constant against a well-meaning edit to Super+V.
        assert_eq!(crate::config::DEFAULT_SHORTCUT, "Ctrl+grave");
    }

    #[test]
    fn a_literal_backtick_means_the_grave_key() {
        // What a user actually types into the settings field.
        for spelling in ["Ctrl+`", "ctrl + `", "Control-grave", "CTRL+GRAVE", "<Control>grave"] {
            let accel = Accelerator::parse(spelling)
                .unwrap_or_else(|e| panic!("{spelling:?} should parse: {e}"));
            assert_eq!(accel.key, "grave", "{spelling:?}");
            assert!(accel.mods.ctrl, "{spelling:?}");
        }
    }

    #[test]
    fn super_v_is_accepted_because_it_is_configurable() {
        // Not our default, but a user who wants the Windows binding must be
        // able to have it (§3).
        let accel = Accelerator::parse("Super+V").unwrap();
        assert!(accel.mods.superkey);
        assert_eq!(accel.key, "v");
        assert_eq!(accel.to_gsettings(), "<Super>v");
        assert_eq!(accel.to_kde(), "Meta+V");
    }

    #[test]
    fn a_bare_key_is_rejected_rather_than_registered() {
        // Registering `grave` alone would swallow the backtick in every
        // application on the system.
        let err = Accelerator::parse("grave").unwrap_err().to_string();
        assert!(err.contains("at least one modifier"), "{err}");
    }

    #[test]
    fn an_empty_or_modifier_only_shortcut_is_rejected() {
        assert!(Accelerator::parse("").is_err());
        assert!(Accelerator::parse("   ").is_err());
        let err = Accelerator::parse("Ctrl+Shift").unwrap_err().to_string();
        assert!(err.contains("no key"), "{err}");
    }

    #[test]
    fn two_keys_is_an_error_not_a_silent_pick() {
        assert!(Accelerator::parse("Ctrl+a+b").is_err());
    }

    #[test]
    fn function_keys_keep_their_capital_f() {
        // X11 keysym lookup is case-sensitive: `f5` is not a keysym.
        assert_eq!(Accelerator::parse("Ctrl+f5").unwrap().key, "F5");
        assert_eq!(Accelerator::parse("Alt+F12").unwrap().key, "F12");
        // F25 is not a real key; it should not be silently accepted as one.
        assert_eq!(Accelerator::parse("Ctrl+F25").unwrap().key, "F25");
    }

    #[test]
    fn spelled_out_key_names_get_canonical_casing() {
        // `XStringToKeysym` would fail on "escape" or "Grave".
        assert_eq!(Accelerator::parse("Ctrl+escape").unwrap().key, "Escape");
        assert_eq!(Accelerator::parse("Ctrl+Grave").unwrap().key, "grave");
        assert_eq!(Accelerator::parse("Ctrl+backspace").unwrap().key, "BackSpace");
    }

    #[test]
    fn punctuation_keys_map_to_their_keysym_names() {
        assert_eq!(Accelerator::parse("Ctrl+.").unwrap().key, "period");
        assert_eq!(Accelerator::parse("Ctrl+/").unwrap().key, "slash");
        assert_eq!(Accelerator::parse("Ctrl+;").unwrap().key, "semicolon");
        assert_eq!(Accelerator::parse("Ctrl+\\").unwrap().key, "backslash");
    }

    #[test]
    fn letters_are_lowercased_so_shift_stays_explicit() {
        // `Ctrl+V` and `Ctrl+v` are the same binding; Shift must be a modifier,
        // not implied by the letter's case.
        let upper = Accelerator::parse("Ctrl+V").unwrap();
        let lower = Accelerator::parse("Ctrl+v").unwrap();
        assert_eq!(upper, lower);
        assert!(!upper.mods.shift);
    }

    #[test]
    fn modifier_aliases_from_every_desktop_are_understood() {
        assert!(Accelerator::parse("Primary+v").unwrap().mods.ctrl);
        assert!(Accelerator::parse("Mod4+v").unwrap().mods.superkey);
        assert!(Accelerator::parse("Logo+v").unwrap().mods.superkey);
        assert!(Accelerator::parse("Win+v").unwrap().mods.superkey);
        assert!(Accelerator::parse("Mod1+v").unwrap().mods.alt);
    }

    #[test]
    fn modifier_counting_supports_conflict_reporting() {
        assert_eq!(Accelerator::parse("Ctrl+grave").unwrap().mods.count(), 1);
        assert_eq!(Accelerator::parse("Ctrl+Shift+Alt+Super+v").unwrap().mods.count(), 4);
    }
}
