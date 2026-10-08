//! The request/response vocabulary spoken over the private socket (§24, §25).
//!
//! Newline-delimited JSON, one object per line, request then response then
//! close. JSON because the messages are tiny and infrequent — a handful per
//! day, driven by a human pressing a key — so a self-describing format that is
//! trivial to debug with `socat` beats a packed one by every measure that
//! matters here.
//!
//! # Not a network protocol
//!
//! This travels over a Unix domain socket in `$XDG_RUNTIME_DIR`, which is mode
//! 0700 and owned by the user. §28 is explicit: clipboard history must not be
//! reachable through a socket other users can open. There is no TCP listener,
//! no authentication layer (the filesystem *is* the authentication), and no
//! remote mode — and because [`Response::Items`] can carry history content,
//! that boundary is doing real work rather than being belt-and-braces.
//!
//! # Compatibility
//!
//! Both enums are `#[serde(tag = "op")]` with `#[serde(other)]`-style handling
//! on the read side via [`Request::parse`], so a newer CLI talking to an older
//! daemon gets a clear "unknown request" rather than a parse failure. The
//! socket is per-session and both ends ship in the same package, so this is
//! defence against a half-finished upgrade, not a versioning scheme.

use serde::{Deserialize, Serialize};

/// Maximum length of one line, to bound what a local process can make the
/// daemon allocate. A search query is the longest thing we expect.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// What the CLI (or a desktop shortcut) asks the daemon to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Open the panel if closed, close it if open. What the shortcut sends.
    Toggle,
    Show,
    Hide,
    /// Delete history. Pinned entries are kept unless `include_pinned`.
    Clear {
        #[serde(default)]
        include_pinned: bool,
    },
    /// Suspend recording. History stays readable (§15).
    Pause,
    Resume,
    /// Everything `clipboard-manager status` prints.
    Status,
    Search {
        query: String,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    /// Open the settings window.
    Settings,
    /// Shut the daemon down cleanly, honouring "clear on exit".
    Quit,
    /// Liveness check used by the single-instance handshake.
    Ping,
}

fn default_limit() -> usize {
    20
}

impl Request {
    /// Parse one line, turning anything unrecognised into an error the caller
    /// can report rather than a panic.
    pub fn parse(line: &str) -> Result<Self, String> {
        serde_json::from_str(line).map_err(|err| format!("unrecognised request: {err}"))
    }

    pub fn encode(&self) -> String {
        // Infallible for these shapes; a failure would be a bug in serde, and
        // an empty line produces a clean parse error at the far end rather
        // than a panic here.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// One history entry, flattened for the CLI.
///
/// Deliberately not [`crate::model::ClipItem`]: that type carries the payload
/// and the content hash, and the CLI has no business receiving either. This
/// carries what `search` needs to print and nothing else (§54).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemSummary {
    pub id: i64,
    pub kind: String,
    pub preview: String,
    pub size_bytes: u64,
    pub is_pinned: bool,
    /// Unix milliseconds.
    pub last_used_at: i64,
}

/// Daemon state, for `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub version: String,
    pub pid: u32,
    /// e.g. "GNOME on Wayland".
    pub session: String,
    /// Which monitoring backend is running.
    pub backend: String,
    /// Backend health, in words.
    pub health: String,
    /// Platform limitation, if the backend has one.
    pub caveat: Option<String>,
    /// The shortcut, and whether it is actually registered (§3).
    pub shortcut: String,
    pub shortcut_working: bool,
    pub paused: bool,
    pub total_items: u64,
    pub pinned_items: u64,
    pub storage_bytes: u64,
    pub storage_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// Succeeded, with an optional line to print.
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Status(Box<StatusReport>),
    Items {
        items: Vec<ItemSummary>,
    },
    Error {
        message: String,
    },
}

impl Response {
    pub fn ok() -> Self {
        Response::Ok { message: None }
    }

    pub fn message(text: impl Into<String>) -> Self {
        Response::Ok { message: Some(text.into()) }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Response::Error { message: text.into() }
    }

    pub fn parse(line: &str) -> Result<Self, String> {
        serde_json::from_str(line).map_err(|err| format!("unrecognised response: {err}"))
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            // Still valid JSON, so the far end gets a parse-able error instead
            // of a truncated stream.
            r#"{"result":"error","message":"the daemon could not encode its reply"}"#.to_string()
        })
    }

    /// Exit status for the CLI: non-zero on error, so scripts can branch.
    pub fn is_error(&self) -> bool {
        matches!(self, Response::Error { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_survives_a_round_trip() {
        let requests = [
            Request::Toggle,
            Request::Show,
            Request::Hide,
            Request::Clear { include_pinned: false },
            Request::Clear { include_pinned: true },
            Request::Pause,
            Request::Resume,
            Request::Status,
            Request::Search { query: "ffmpeg".into(), limit: 20 },
            Request::Settings,
            Request::Quit,
            Request::Ping,
        ];
        for request in requests {
            let encoded = request.encode();
            assert!(!encoded.contains('\n'), "encoding must stay on one line: {encoded}");
            assert_eq!(Request::parse(&encoded).unwrap(), request, "{encoded}");
        }
    }

    #[test]
    fn the_wire_form_is_stable_and_readable() {
        // Pinned deliberately: this is what a desktop shortcut and any script
        // will be sending, so a rename here is a breaking change.
        assert_eq!(Request::Toggle.encode(), r#"{"op":"toggle"}"#);
        assert_eq!(Request::Ping.encode(), r#"{"op":"ping"}"#);
    }

    #[test]
    fn a_search_without_a_limit_gets_a_sensible_default() {
        let request = Request::parse(r#"{"op":"search","query":"nginx"}"#).unwrap();
        assert_eq!(request, Request::Search { query: "nginx".into(), limit: 20 });
    }

    #[test]
    fn clear_defaults_to_keeping_pinned_entries() {
        // §12: a pin outlives cleanup, so the risky flag must be opt-in.
        let request = Request::parse(r#"{"op":"clear"}"#).unwrap();
        assert_eq!(request, Request::Clear { include_pinned: false });
    }

    #[test]
    fn an_unknown_request_is_an_error_not_a_panic() {
        for line in [
            r#"{"op":"self_destruct"}"#,
            r#"{"nonsense":true}"#,
            "not json at all",
            "",
        ] {
            assert!(Request::parse(line).is_err(), "{line:?} should not parse");
        }
    }

    #[test]
    fn responses_round_trip_including_the_status_report() {
        let report = StatusReport {
            version: "1.0.0".into(),
            pid: 42,
            session: "GNOME on Wayland".into(),
            backend: "x11".into(),
            health: "running".into(),
            caveat: Some("collected through XWayland".into()),
            shortcut: "Ctrl + ` (backtick)".into(),
            shortcut_working: true,
            paused: false,
            total_items: 7,
            pinned_items: 1,
            storage_bytes: 2048,
            storage_path: "/home/u/.local/share/clipboard-manager".into(),
        };
        let response = Response::Status(Box::new(report.clone()));
        let decoded = Response::parse(&response.encode()).unwrap();
        assert_eq!(decoded, response);

        match decoded {
            Response::Status(decoded) => assert_eq!(*decoded, report),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn errors_are_distinguishable_so_the_cli_can_set_an_exit_code() {
        assert!(Response::error("broken").is_error());
        assert!(!Response::ok().is_error());
        assert!(!Response::message("done").is_error());
    }

    #[test]
    fn item_summaries_carry_no_payload_or_hash() {
        // §54: the CLI gets previews, never content hashes or blobs.
        let json = serde_json::to_string(&ItemSummary {
            id: 1,
            kind: "text".into(),
            preview: "hello".into(),
            size_bytes: 5,
            is_pinned: false,
            last_used_at: 0,
        })
        .unwrap();
        assert!(!json.contains("content_hash"), "{json}");
        assert!(!json.contains("blob"), "{json}");
        assert!(!json.contains("text_content"), "{json}");
    }

    #[test]
    fn the_line_limit_is_large_enough_for_a_real_search_but_still_bounded() {
        // Compared through a runtime binding: `assert!` on two constants is
        // folded away at compile time and tests nothing.
        let limit = MAX_LINE_BYTES;
        assert!(limit >= 4096, "too small for a real search query: {limit}");
        assert!(limit <= 1024 * 1024, "unbounded enough to be a memory risk: {limit}");
    }
}
