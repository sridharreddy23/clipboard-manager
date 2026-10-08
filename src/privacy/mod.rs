//! Deciding what must *not* be stored (§16, §17).
//!
//! Three independent mechanisms, in descending order of reliability:
//!
//! 1. **Password-manager hints.** A source application can mark a selection as
//!    a secret by advertising an extra clipboard target. This is exact — there
//!    are no false positives — but it only works for applications that bother,
//!    which in practice means KDE-aware ones (KeePassXC, KWallet, Firefox under
//!    Plasma) offering `x-kde-passwordManagerHint`.
//!
//! 2. **Application exclusion.** Matching the owning application against a user
//!    list. Reliable under X11. **Not** reliable under Wayland, where no
//!    protocol in the stack tells us who owns a selection: there, the owning
//!    application is simply unknown and an exclusion rule cannot fire. This is
//!    a property of the Wayland security model, not a bug here, and the
//!    settings UI says so rather than implying the list always works.
//!
//! 3. **Content heuristics.** Guessing from the text that something looks like
//!    a generated credential. This one *can* have false positives, so it is a
//!    separate setting that defaults to off: silently discarding an API key the
//!    user meant to keep would be a worse failure than storing one in a
//!    `0600` database on their own machine.
//!
//! Nothing in this module logs the content it inspects.

use crate::config::Privacy;
use crate::model::AppIdentity;

/// Clipboard targets that mean "this selection is a secret".
///
/// `x-kde-passwordManagerHint` is the only one of these with any real adoption;
/// it carries the value `secret`, but applications only advertise it at all
/// when flagging a password, so its presence is sufficient. The rest are
/// accepted because they cost nothing and some forks use them.
pub const SECRET_HINT_TARGETS: &[&str] = &[
    "x-kde-passwordManagerHint",
    "application/x-kde-passwordManagerHint",
    "x-kde-passwordmanagerhint",
    "application/x-secret",
    "x-moz-password",
];

/// Why a clipping was not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// The source application flagged the selection as a secret.
    SecretHint,
    /// The source application matched a user exclusion rule. Carries the
    /// pattern that matched, so the UI can say which rule fired.
    ExcludedApp { pattern: String },
    /// The text matched the credential heuristic.
    SecretLikeText,
}

impl Block {
    pub fn reason(&self) -> String {
        match self {
            Block::SecretHint => "source application marked it as a password".to_string(),
            Block::ExcludedApp { pattern } => format!("source application matches \"{pattern}\""),
            Block::SecretLikeText => "looks like a generated credential".to_string(),
        }
    }
}

/// Should this clipping be dropped before it is ever stored?
///
/// Called on the monitor thread with the retrieved payload but *before*
/// normalisation, so a blocked clipping never reaches the blob store either.
pub fn check(
    settings: &Privacy,
    offered: &[String],
    app: Option<&AppIdentity>,
    text: Option<&str>,
) -> Option<Block> {
    if settings.skip_passwords && has_secret_hint(offered) {
        return Some(Block::SecretHint);
    }
    if let Some(app) = app {
        if let Some(pattern) = matching_exclusion(&settings.excluded_apps, app) {
            return Some(Block::ExcludedApp { pattern });
        }
    }
    if settings.skip_secret_like_text {
        if let Some(text) = text {
            if looks_like_credential(text) {
                return Some(Block::SecretLikeText);
            }
        }
    }
    None
}

/// True when any offered target marks the selection as a secret.
pub fn has_secret_hint(offered: &[String]) -> bool {
    offered.iter().any(|target| {
        SECRET_HINT_TARGETS
            .iter()
            .any(|hint| target.eq_ignore_ascii_case(hint))
    })
}

/// The first exclusion pattern matching this application, if any.
///
/// `patterns` are expected already lower-cased and trimmed by
/// [`crate::config::Settings::normalized`]. Matching is case-insensitive
/// substring matching so that `keepassxc` covers `/usr/bin/keepassxc`,
/// `keepassxc.desktop` and the `keepassxc` `WM_CLASS` alike.
pub fn matching_exclusion(patterns: &[String], app: &AppIdentity) -> Option<String> {
    if patterns.is_empty() || app.is_empty() {
        return None;
    }
    let haystacks: Vec<String> = app.haystacks().map(|h| h.to_lowercase()).collect();
    patterns
        .iter()
        .find(|pattern| {
            let needle = pattern.to_lowercase();
            !needle.is_empty() && haystacks.iter().any(|h| h.contains(&needle))
        })
        .cloned()
}

/// Heuristic: does this text look like a machine-generated credential?
///
/// Tuned to be *specific* rather than sensitive, because a false positive
/// silently loses data the user asked to keep. It requires all of:
///
/// * a single line with no whitespace at all;
/// * a length in the range typical of generated secrets;
/// * at least three of the four character classes, which excludes hex digests,
///   base-32 keys, UUIDs, commit SHAs and ordinary identifiers;
/// * a high proportion of distinct characters, which excludes repetitive
///   placeholder strings;
/// * not being a URL, a path, an email address or a `key=value` pair.
///
/// Even so this is a guess, which is why it is opt-in. It is never applied to
/// images or file selections.
pub fn looks_like_credential(text: &str) -> bool {
    let s = text.trim();
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }

    let chars: Vec<char> = s.chars().collect();
    if !(12..=128).contains(&chars.len()) {
        return false;
    }

    // Structured values that merely look dense.
    let lower = s.to_ascii_lowercase();
    if lower.contains("://")
        || lower.starts_with('/')
        || lower.starts_with("~/")
        || lower.starts_with("./")
        || lower.starts_with("c:\\")
        || is_email_like(s)
        || is_assignment(s)
        || is_uuid(s)
    {
        return false;
    }

    let has_lower = chars.iter().any(|c| c.is_lowercase());
    let has_upper = chars.iter().any(|c| c.is_uppercase());
    let has_digit = chars.iter().any(|c| c.is_ascii_digit());
    let has_symbol = chars
        .iter()
        .any(|c| c.is_ascii_punctuation() && *c != '-' && *c != '_' && *c != '.');
    let classes =
        [has_lower, has_upper, has_digit, has_symbol].iter().filter(|c| **c).count();
    if classes < 3 {
        return false;
    }

    // Distinct-character ratio, as a cheap stand-in for entropy.
    let mut distinct: Vec<char> = chars.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let ratio = distinct.len() as f64 / chars.len() as f64;
    if ratio < 0.6 {
        return false;
    }

    // Dictionary-ish text separated by `-`/`_`/`.` (for example a hyphenated
    // passphrase or a dotted package name) is usually not a secret.
    let words: Vec<&str> = s.split(['-', '_', '.']).filter(|w| !w.is_empty()).collect();
    if words.len() >= 3 && words.iter().all(|w| w.chars().all(|c| c.is_alphabetic())) {
        return false;
    }

    true
}

/// `user@host.tld` shape. Deliberately requires a dot in the domain, so that a
/// password which happens to contain `@` is not excused.
fn is_email_like(s: &str) -> bool {
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        && domain.contains('.')
        && domain.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '-'))
}

/// `KEY=value` shape. Trailing `=` is ignored so that base64 padding does not
/// make a token look like an assignment.
fn is_assignment(s: &str) -> bool {
    s.trim_end_matches('=').contains('=')
}

fn is_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn privacy() -> Privacy {
        Privacy::default()
    }

    fn app(name: &str) -> AppIdentity {
        AppIdentity { name: Some(name.to_string()), ..Default::default() }
    }

    #[test]
    fn the_kde_password_hint_blocks_a_clipping() {
        let offered = vec![
            "text/plain".to_string(),
            "x-kde-passwordManagerHint".to_string(),
        ];
        assert_eq!(
            check(&privacy(), &offered, None, Some("hunter2")),
            Some(Block::SecretHint)
        );
    }

    #[test]
    fn the_hint_is_matched_case_insensitively() {
        assert!(has_secret_hint(&["X-KDE-PASSWORDMANAGERHINT".to_string()]));
        assert!(!has_secret_hint(&["text/plain".to_string()]));
        assert!(!has_secret_hint(&[]));
    }

    #[test]
    fn the_hint_can_be_switched_off() {
        let mut p = privacy();
        p.skip_passwords = false;
        let offered = vec!["x-kde-passwordManagerHint".to_string()];
        assert_eq!(check(&p, &offered, None, Some("hunter2")), None);
    }

    #[test]
    fn default_exclusions_cover_the_common_password_managers() {
        let p = Privacy::default();
        for name in ["keepassxc", "bitwarden", "1password"] {
            let blocked = check(&p, &[], Some(&app(name)), Some("secret"));
            assert!(matches!(blocked, Some(Block::ExcludedApp { .. })), "{name} not excluded");
        }
    }

    #[test]
    fn exclusion_matches_substrings_of_every_identity_field() {
        let patterns = vec!["keepassxc".to_string()];

        let by_exe = AppIdentity {
            exe: Some("/usr/lib/keepassxc/keepassxc".into()),
            ..Default::default()
        };
        assert_eq!(matching_exclusion(&patterns, &by_exe).as_deref(), Some("keepassxc"));

        let by_class =
            AppIdentity { wm_class: Some("KeePassXC".into()), ..Default::default() };
        assert_eq!(matching_exclusion(&patterns, &by_class).as_deref(), Some("keepassxc"));

        let unrelated = app("gnome-terminal");
        assert_eq!(matching_exclusion(&patterns, &unrelated), None);
    }

    #[test]
    fn an_unknown_application_cannot_match_an_exclusion() {
        // This is the honest Wayland case: no identity, so no rule can fire.
        let p = Privacy::default();
        assert_eq!(check(&p, &[], None, Some("whatever")), None);
        assert_eq!(matching_exclusion(&p.excluded_apps, &AppIdentity::default()), None);
    }

    #[test]
    fn empty_pattern_list_matches_nothing() {
        assert_eq!(matching_exclusion(&[], &app("keepassxc")), None);
    }

    #[test]
    fn the_content_heuristic_is_off_by_default() {
        let p = Privacy::default();
        assert!(!p.skip_secret_like_text);
        assert_eq!(check(&p, &[], None, Some("Tr0ub4dor&3xKp!")), None);
    }

    #[test]
    fn the_content_heuristic_catches_generated_passwords_when_enabled() {
        let mut p = privacy();
        p.skip_secret_like_text = true;
        for secret in [
            "Tr0ub4dor&3xKp!",
            "xK9#mQ2$vL7@pR4z",
            "aB3!cD5%eF7&gH9*",
            "P@ssw0rd!2024#xyz",
        ] {
            assert_eq!(
                check(&p, &[], None, Some(secret)),
                Some(Block::SecretLikeText),
                "{secret}"
            );
        }
    }

    #[test]
    fn the_content_heuristic_leaves_ordinary_clippings_alone() {
        for text in [
            // Commands and prose: contain whitespace.
            "sudo systemctl restart nginx",
            "ffmpeg -i input.mp4 output.ts",
            "Hello World",
            // Too short.
            "abc123!",
            // Two classes only: digests, SHAs, base-32 keys.
            "d41d8cd98f00b204e9800998ecf8427e",
            "a3f1c9e8b7d6a5f4e3c2b1a09f8e7d6c5b4a3928",
            "JBSWY3DPEHPK3PXP",
            "ABCDEFGHIJKLMNOP",
            // Structured values.
            "550e8400-e29b-41d4-a716-446655440000",
            "https://example.com/a/b?c=d",
            "/home/user/Documents/report.pdf",
            "someone@example.com",
            "DATABASE_URL=postgres://localhost",
            // Hyphenated passphrase.
            "correct-horse-battery-staple",
            // Repetitive.
            "aaaaaaaaaaaaaaaaaaaa!A1",
            // Multi-line.
            "line one\nline two\nline three",
            "",
        ] {
            assert!(!looks_like_credential(text), "false positive on {text:?}");
        }
    }

    #[test]
    fn the_heuristic_never_panics_on_awkward_input() {
        for text in ["日本語のテキストです", "👍🏽🎉🚀👍🏽🎉🚀👍🏽🎉🚀", "\0\0\0", " "] {
            let _ = looks_like_credential(text);
        }
    }

    #[test]
    fn hints_win_over_exclusions_in_the_reported_reason() {
        // Both would fire; the exact mechanism is reported.
        let p = Privacy::default();
        let offered = vec!["x-kde-passwordManagerHint".to_string()];
        assert_eq!(
            check(&p, &offered, Some(&app("keepassxc")), None),
            Some(Block::SecretHint)
        );
    }

    #[test]
    fn block_reasons_name_the_matching_rule() {
        let block = Block::ExcludedApp { pattern: "bitwarden".into() };
        assert!(block.reason().contains("bitwarden"));
        assert!(Block::SecretHint.reason().contains("password"));
    }
}
