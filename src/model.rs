//! Core domain types shared by the platform layer, the storage layer and the UI.
//!
//! Nothing in this module performs I/O, so it is cheap to test and safe to use
//! from any thread.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which X11/Wayland selection a clipping came from.
///
/// Keeping these distinct is a hard requirement (§19): PRIMARY is the
/// middle-click selection and changes constantly as the user drags over text,
/// whereas CLIPBOARD is an explicit copy. They are never merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Selection {
    Clipboard,
    Primary,
}

impl Selection {
    pub fn as_str(self) -> &'static str {
        match self {
            Selection::Clipboard => "clipboard",
            Selection::Primary => "primary",
        }
    }
}

/// How a clipping is classified for display and for restoration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    /// Plain text, possibly multi-line.
    Text,
    /// Text that is a single well-formed URL. Stored byte-identically to
    /// [`ClipKind::Text`]; the distinction only affects presentation.
    Url,
    /// A raster image.
    Image,
    /// One or more file paths (a `text/uri-list` selection).
    Files,
}

impl ClipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClipKind::Text => "text",
            ClipKind::Url => "url",
            ClipKind::Image => "image",
            ClipKind::Files => "files",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(ClipKind::Text),
            "url" => Some(ClipKind::Url),
            "image" => Some(ClipKind::Image),
            "files" => Some(ClipKind::Files),
            _ => None,
        }
    }

    /// True for kinds whose payload is binary and is therefore always kept in
    /// the blob store rather than inline (§15).
    ///
    /// Note that text can *also* end up in the blob store when it is very
    /// large, so storage decisions are made from
    /// [`ClipItem::blob_hash`]/[`NewClip::blob_hash`] rather than from the
    /// kind. Use [`ClipKind::is_editable_text`] to ask whether an entry can be
    /// edited as text.
    pub fn is_binary(self) -> bool {
        matches!(self, ClipKind::Image | ClipKind::Files)
    }

    /// True for kinds the user can edit in the text editor dialog (§13).
    pub fn is_editable_text(self) -> bool {
        matches!(self, ClipKind::Text | ClipKind::Url)
    }

    /// Human label used in the UI and in `status` output.
    pub fn label(self) -> &'static str {
        match self {
            ClipKind::Text => "Text",
            ClipKind::Url => "Link",
            ClipKind::Image => "Image",
            ClipKind::Files => "Files",
        }
    }

    /// Icon name from the freedesktop icon-naming spec, with widely available
    /// fallbacks handled by the UI layer.
    pub fn icon_name(self) -> &'static str {
        match self {
            ClipKind::Text => "text-x-generic-symbolic",
            ClipKind::Url => "insert-link-symbolic",
            ClipKind::Image => "image-x-generic-symbolic",
            ClipKind::Files => "folder-symbolic",
        }
    }
}

impl fmt::Display for ClipKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Dimensions and format of a stored image, recorded once so the UI never has
/// to decode a blob just to lay out a row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMeta {
    pub width: u32,
    pub height: u32,
    /// Short format tag, e.g. `png`, `jpeg`, `webp`.
    pub format: String,
}

/// A single entry of a file selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// Absolute path, when the URI was a `file://` URI.
    pub path: String,
    /// Basename, used as the row title.
    pub name: String,
    /// `None` when the file could not be stat-ed (e.g. it has since been moved).
    pub size: Option<u64>,
    pub is_dir: bool,
}

impl FileMeta {
    pub fn from_path(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let meta = std::fs::metadata(&path).ok();
        Self {
            path: path.to_string_lossy().into_owned(),
            name,
            size: meta.as_ref().map(|m| m.len()),
            is_dir: meta.map(|m| m.is_dir()).unwrap_or(false),
        }
    }
}

/// Per-item metadata, serialised to JSON in a single database column.
///
/// Everything here is derived from the clipping itself or from the window
/// manager; none of it is sensitive beyond what the payload already is, and
/// none of it is ever logged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// Best-effort identification of the application that owned the selection.
    /// Always `None` under Wayland, where no protocol exposes it (§17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageMeta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub char_count: Option<u32>,
    /// Every MIME type the source offered. Kept for diagnostics and for
    /// choosing a better representation in a future version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offered_mime: Vec<String>,
    /// Set when the clipping came from PRIMARY rather than CLIPBOARD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<Selection>,
    /// Set when the user has edited the text of this entry (§13).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
}

impl Metadata {
    /// Extra text folded into the search index so that searching a file name or
    /// an image format finds the entry (§7).
    pub fn search_text(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(app) = &self.source_app {
            parts.push(app);
        }
        if let Some(img) = &self.image {
            parts.push(&img.format);
        }
        for f in &self.files {
            parts.push(&f.name);
            parts.push(&f.path);
        }
        parts.join(" ")
    }
}

/// Best-effort identification of the application that owned a selection.
///
/// Every field is independently optional because every field comes from a
/// different, independently unreliable source: `name` and `exe` need a PID we
/// can only get from an X11 property, and `wm_class` needs the owner to have a
/// mapped top-level window. Under Wayland all three are normally `None`, since
/// no protocol in the stack exposes the owner of a selection (§17).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppIdentity {
    /// Process name, from `/proc/<pid>/comm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Executable path, from `/proc/<pid>/exe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// `WM_CLASS` of the owning window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wm_class: Option<String>,
}

impl AppIdentity {
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.exe.is_none() && self.wm_class.is_none()
    }

    /// Short name for display and for `Metadata::source_app`.
    pub fn label(&self) -> Option<String> {
        if let Some(name) = &self.name {
            return Some(name.clone());
        }
        if let Some(class) = &self.wm_class {
            return Some(class.clone());
        }
        self.exe.as_deref().map(|exe| {
            std::path::Path::new(exe)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| exe.to_string())
        })
    }

    /// Every string an exclusion pattern may be matched against (§16).
    pub fn haystacks(&self) -> impl Iterator<Item = &str> {
        [self.name.as_deref(), self.exe.as_deref(), self.wm_class.as_deref()]
            .into_iter()
            .flatten()
    }
}

/// A clipping that has been read from the display server but not yet stored.
#[derive(Clone)]
pub struct RawClip {
    /// All MIME types the owner advertised, in the order advertised.
    pub offered: Vec<String>,
    /// The MIME type we chose to retrieve.
    pub mime: String,
    /// The retrieved bytes.
    pub bytes: Vec<u8>,
    pub selection: Selection,
    pub source_app: Option<AppIdentity>,
}

impl fmt::Debug for RawClip {
    /// Deliberately omits `bytes`: a `Debug` impl that printed the payload
    /// would make it one careless `tracing` call away from being written to the
    /// journal (§36).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawClip")
            .field("mime", &self.mime)
            .field("offered", &self.offered)
            .field("len", &self.bytes.len())
            .field("selection", &self.selection)
            .field("source_app", &self.source_app)
            .finish()
    }
}

/// A clipping after normalisation, ready to be inserted.
#[derive(Clone)]
pub struct NewClip {
    pub kind: ClipKind,
    /// The MIME type that will be re-offered when this item is restored, so a
    /// round trip is byte-for-byte faithful (§52).
    pub mime_type: String,
    pub content_hash: String,
    /// Inline payload for text-like kinds.
    pub text_content: Option<String>,
    /// For blob-backed kinds: the hash of a payload that has **already been
    /// written** to the blob store by the monitor thread.
    ///
    /// Writing the blob off the main thread is what keeps the UI responsive
    /// when a 50 MB screenshot is copied (§33). If the daemon dies between the
    /// blob write and the database commit the blob is simply unreferenced, and
    /// maintenance garbage-collects it (§48).
    pub blob_hash: Option<String>,
    pub preview: String,
    pub metadata: Metadata,
    pub size_bytes: u64,
}

impl fmt::Debug for NewClip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewClip")
            .field("kind", &self.kind)
            .field("mime_type", &self.mime_type)
            .field("hash", &short_hash(&self.content_hash))
            .field("size_bytes", &self.size_bytes)
            .finish()
    }
}

/// A stored clipboard entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipItem {
    pub id: i64,
    pub kind: ClipKind,
    pub mime_type: String,
    pub content_hash: String,
    pub text_content: Option<String>,
    pub blob_hash: Option<String>,
    pub preview: String,
    pub metadata: Metadata,
    pub size_bytes: u64,
    /// Unix milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: i64,
    pub is_pinned: bool,
}

impl ClipItem {
    /// Single-line title for the row, with control characters folded to spaces
    /// so a multi-line clipping cannot break the layout.
    pub fn title(&self) -> String {
        match self.kind {
            ClipKind::Image => {
                let dims = self
                    .metadata
                    .image
                    .as_ref()
                    .map(|i| format!("{} × {}", i.width, i.height))
                    .unwrap_or_else(|| "Image".to_string());
                format!("Image · {dims}")
            }
            ClipKind::Files => match self.metadata.files.len() {
                0 => "Files".to_string(),
                1 => self.metadata.files[0].name.clone(),
                n => format!("{} and {} more", self.metadata.files[0].name, n - 1),
            },
            _ => collapse_whitespace(&self.preview),
        }
    }

    /// Secondary line: type, size, and relative time.
    pub fn subtitle(&self, now_ms: i64, show_timestamp: bool) -> String {
        let mut parts = vec![self.kind.label().to_string()];
        match self.kind {
            ClipKind::Text | ClipKind::Url => {
                if let Some(lines) = self.metadata.line_count.filter(|l| *l > 1) {
                    parts.push(format!("{lines} lines"));
                }
            }
            _ => parts.push(format_size(self.size_bytes)),
        }
        if self.metadata.edited {
            parts.push("edited".to_string());
        }
        if show_timestamp {
            parts.push(relative_time(self.last_used_at, now_ms));
        }
        parts.join(" · ")
    }

    /// True when the row should offer "View full content" (§32).
    ///
    /// Computed from stored metadata rather than from `text_content`, because
    /// list queries deliberately do not load the full text (see
    /// [`crate::storage::Store::recent`]).
    pub fn has_more_content(&self) -> bool {
        if !self.kind.is_editable_text() {
            return true;
        }
        !self.preview_covers_content() || self.metadata.line_count.map(|l| l > 1).unwrap_or(false)
    }

    /// Characters of *content* in the preview, excluding the trailing ellipsis
    /// that [`truncate_chars`] appends.
    pub fn preview_char_len(&self) -> u32 {
        let total = self.preview.chars().count();
        let elided = usize::from(self.preview.ends_with('…'));
        total.saturating_sub(elided) as u32
    }

    /// True when the preview and metadata together hold everything about this
    /// entry that search could possibly match.
    ///
    /// For images and file selections that is always the case: their payload is
    /// binary, and every searchable string (dimensions, format, file names)
    /// already lives in the preview or the metadata. For text it depends on
    /// whether the preview was truncated.
    ///
    /// Used by [`crate::storage::Store::search`] to tell a genuine non-match
    /// apart from a match that is simply beyond the preview.
    pub fn preview_covers_content(&self) -> bool {
        if !self.kind.is_editable_text() {
            return true;
        }
        match self.metadata.char_count {
            Some(chars) => chars <= self.preview_char_len(),
            // Written by an older version that did not record the count; assume
            // there may be more, so a candidate is never wrongly discarded.
            None => false,
        }
    }

    /// True when the full payload is not loaded in this value.
    ///
    /// Rows that come from a list or search query carry only the preview; the
    /// caller must go back to the store for the payload.
    pub fn is_preview_only(&self) -> bool {
        self.text_content.is_none() && self.blob_hash.is_none()
    }
}

/// Collapse all whitespace runs (including newlines and tabs) into single
/// spaces. Used for row titles only — stored content is never modified.
pub fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() || ch.is_control() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Truncate to `max` **characters** (never bytes, so multi-byte scripts and
/// emoji are never split) and append an ellipsis when something was removed.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    for (count, (idx, _)) in s.char_indices().enumerate() {
        if count == max {
            return format!("{}…", &s[..idx]);
        }
    }
    s.to_string()
}

/// First 12 hex characters of a content hash — enough to correlate log lines
/// without being reversible to content.
pub fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Binary-ish human size, matching what file managers show.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

/// Current wall-clock time in unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Windows-Clipboard-History-style relative timestamp.
pub fn relative_time(then_ms: i64, now_ms: i64) -> String {
    let delta = (now_ms - then_ms).max(0) / 1000;
    match delta {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        86_400..=604_799 => {
            let d = delta / 86_400;
            if d == 1 {
                "yesterday".to_string()
            } else {
                format!("{d}d ago")
            }
        }
        _ => {
            let w = delta / 604_800;
            if w < 5 {
                format!("{w}w ago")
            } else {
                format!("{}mo ago", delta / 2_592_000)
            }
        }
    }
}

/// Absolute timestamp for tooltips and exports (local time, ISO-like).
pub fn format_absolute(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|d| d.format("%Y-%m-%d %H:%M:%S"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| format!("{ms}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_collapses_without_losing_unicode() {
        assert_eq!(collapse_whitespace("a\n\tb   c"), "a b c");
        assert_eq!(collapse_whitespace("  héllo\u{00a0}wörld "), "héllo wörld");
        assert_eq!(collapse_whitespace("日本\n語"), "日本 語");
        assert_eq!(collapse_whitespace("\n\n\n"), "");
    }

    #[test]
    fn truncate_never_splits_a_codepoint() {
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("héllo wörld", 5), "héllo…");
        // Emoji are multi-byte; slicing by byte here would panic.
        assert_eq!(truncate_chars("👍🏽🎉🚀", 2), "👍🏽…".replace('\u{200b}', ""));
        assert_eq!(truncate_chars("", 5), "");
        assert_eq!(truncate_chars("abc", 0), "");
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn relative_time_buckets() {
        let now = 10_000_000_000i64;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - 30_000, now), "30s ago");
        assert_eq!(relative_time(now - 120_000, now), "2m ago");
        assert_eq!(relative_time(now - 7_200_000, now), "2h ago");
        assert_eq!(relative_time(now - 86_400_000, now), "yesterday");
        assert_eq!(relative_time(now - 3 * 86_400_000, now), "3d ago");
        // A clock that jumped backwards must not produce a negative bucket.
        assert_eq!(relative_time(now + 5_000, now), "just now");
    }

    #[test]
    fn kinds_round_trip() {
        for k in [ClipKind::Text, ClipKind::Url, ClipKind::Image, ClipKind::Files] {
            assert_eq!(ClipKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ClipKind::parse("nonsense"), None);
    }

    #[test]
    fn app_identity_prefers_the_process_name_then_class_then_exe() {
        let full = AppIdentity {
            name: Some("keepassxc".into()),
            exe: Some("/usr/bin/keepassxc".into()),
            wm_class: Some("keepassxc".into()),
        };
        assert_eq!(full.label().as_deref(), Some("keepassxc"));

        let class_only = AppIdentity { wm_class: Some("Navigator".into()), ..Default::default() };
        assert_eq!(class_only.label().as_deref(), Some("Navigator"));

        let exe_only =
            AppIdentity { exe: Some("/opt/Bitwarden/bitwarden".into()), ..Default::default() };
        assert_eq!(exe_only.label().as_deref(), Some("bitwarden"));

        let empty = AppIdentity::default();
        assert!(empty.is_empty());
        assert_eq!(empty.label(), None);
        assert_eq!(full.haystacks().count(), 3);
    }

    #[test]
    fn raw_clip_debug_does_not_leak_content() {
        let clip = RawClip {
            offered: vec!["text/plain".into()],
            mime: "text/plain".into(),
            bytes: b"super-secret-password".to_vec(),
            selection: Selection::Clipboard,
            source_app: None,
        };
        let rendered = format!("{clip:?}");
        assert!(!rendered.contains("secret"), "payload leaked into Debug: {rendered}");
        assert!(rendered.contains("len: 21"));
    }
}
