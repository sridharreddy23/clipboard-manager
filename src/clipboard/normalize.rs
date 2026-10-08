//! Turning a raw selection into a history entry (§8, §9, §10, §33).
//!
//! The display server hands us a *set* of MIME types the owner can produce. We
//! pick exactly one, retrieve it, classify it, and derive a preview. Everything
//! here runs on the clipboard monitor thread, including the SHA-256 and the
//! blob write, so the GTK main thread only ever sees a small struct (§45).

use anyhow::{bail, Result};

use crate::config::Settings;
use crate::model::{
    self, ClipKind, FileMeta, ImageMeta, Metadata, NewClip, RawClip, Selection,
};
use crate::storage::{blobs, BlobStore, INLINE_TEXT_LIMIT};

/// X11 selection targets that describe the selection rather than carrying it.
/// Asking for one of these would succeed and give us nonsense.
const META_TARGETS: &[&str] = &[
    "TARGETS",
    "TIMESTAMP",
    "MULTIPLE",
    "SAVE_TARGETS",
    "DELETE",
    "INSERT_SELECTION",
    "INSERT_PROPERTY",
    "ATOM",
    "ATOM_PAIR",
    "PIXMAP",
    "BITMAP",
    "OWNER_OS",
    "HOST_NAME",
    "USER",
    "CLASS",
    "NAME",
    "LENGTH",
    "FILE_NAME",
    "CHARACTER_POSITION",
    "LINE_NUMBER",
    "COLUMN_NUMBER",
];

/// File-selection targets, most informative first.
///
/// Checked **before** images because file managers offer a thumbnail alongside
/// the URI list, and the user copied files, not a picture of them.
const FILE_TARGETS: &[&str] = &[
    "x-special/gnome-copied-files",
    "x-special/nautilus-clipboard",
    "text/uri-list",
    "application/x-kde-cutselection",
];

/// Image targets, best first. PNG leads because it is lossless and universally
/// understood; WebP before JPEG because it is usually what modern screenshot
/// tools offer alongside PNG.
const IMAGE_TARGETS: &[&str] = &[
    "image/png",
    "image/webp",
    "image/jpeg",
    "image/jpg",
    "image/tiff",
    "image/bmp",
    "image/x-ms-bmp",
    "image/gif",
    "image/x-icon",
    "image/svg+xml",
];

/// Text targets, best first. UTF-8 forms lead so we never have to guess an
/// encoding.
const TEXT_TARGETS: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "text/plain;charset=utf8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
    "COMPOUND_TEXT",
];

/// What kind of payload a chosen target will produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetClass {
    Files,
    Image,
    Text,
}

/// A target we intend to retrieve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChosenTarget {
    pub mime: String,
    pub class: TargetClass,
}

/// Why a clipping was not stored. Surfaced to the user for the cases they can
/// act on, and logged (without content) for the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Empty or whitespace-only selection.
    Empty,
    /// Over the configured per-item size limit (§33).
    TooLarge { size: u64, limit: u64 },
    /// Images are disabled in settings.
    ImagesDisabled,
    /// File selections are disabled in settings.
    FilesDisabled,
    /// The bytes were not valid UTF-8 and the target claimed to be text.
    NotUtf8,
}

impl Rejection {
    /// Message shown in a notification, or `None` when the rejection is
    /// routine and a notification would be noise (§34).
    pub fn user_message(&self) -> Option<String> {
        match self {
            Rejection::TooLarge { size, limit } => Some(format!(
                "Clipboard item not saved: {} exceeds the {} limit.",
                model::format_size(*size),
                model::format_size(*limit)
            )),
            _ => None,
        }
    }

    pub fn reason(&self) -> &'static str {
        match self {
            Rejection::Empty => "empty selection",
            Rejection::TooLarge { .. } => "over size limit",
            Rejection::ImagesDisabled => "images disabled",
            Rejection::FilesDisabled => "files disabled",
            Rejection::NotUtf8 => "not valid UTF-8",
        }
    }
}

/// Choose which offered target to retrieve.
///
/// Returns `None` when nothing usable is on offer, which happens routinely —
/// for example when an application owns the selection only to advertise
/// `TARGETS` and `TIMESTAMP`.
pub fn choose_target(offered: &[String], settings: &Settings) -> Option<ChosenTarget> {
    let has = |name: &str| {
        offered
            .iter()
            .find(|o| o.eq_ignore_ascii_case(name))
            .map(|o| o.to_string())
    };

    if settings.clipboard.store_files {
        for candidate in FILE_TARGETS {
            if let Some(mime) = has(candidate) {
                return Some(ChosenTarget { mime, class: TargetClass::Files });
            }
        }
    }

    if settings.clipboard.store_images {
        for candidate in IMAGE_TARGETS {
            if let Some(mime) = has(candidate) {
                return Some(ChosenTarget { mime, class: TargetClass::Image });
            }
        }
    }

    for candidate in TEXT_TARGETS {
        if let Some(mime) = has(candidate) {
            return Some(ChosenTarget { mime, class: TargetClass::Text });
        }
    }

    // Last resort: an unrecognised `text/*` type (for example
    // `text/plain;charset=ISO-8859-1`). We still only accept it if the bytes
    // turn out to be valid UTF-8, which `normalize` checks.
    offered
        .iter()
        .find(|o| {
            let lower = o.to_ascii_lowercase();
            lower.starts_with("text/")
                && !lower.starts_with("text/html")
                && !lower.starts_with("text/richtext")
                && !lower.starts_with("text/rtf")
                && !META_TARGETS.iter().any(|m| m.eq_ignore_ascii_case(o))
        })
        .map(|mime| ChosenTarget { mime: mime.clone(), class: TargetClass::Text })
}

/// True for a target that only describes the selection.
pub fn is_meta_target(name: &str) -> bool {
    META_TARGETS.iter().any(|m| m.eq_ignore_ascii_case(name))
        || name.starts_with("_NET")
        || name.starts_with("_GTK")
        || name == "MULTIPLE"
}

/// Classify a text payload as plain text or a URL (§8).
///
/// Only a *single* token that looks like an absolute URI counts. This is
/// deliberately strict: misclassifying a paragraph that happens to contain a
/// link would change how it is displayed for no benefit.
pub fn classify_text(text: &str) -> ClipKind {
    if is_url(text.trim()) {
        ClipKind::Url
    } else {
        ClipKind::Text
    }
}

/// Schemes recognised as URLs.
const URL_SCHEMES: &[&str] = &[
    "http", "https", "ftp", "ftps", "sftp", "ssh", "git", "magnet", "mailto", "file", "irc",
    "ircs", "news", "nntp", "rtsp", "rtmp", "srt", "udp", "tel", "sms", "data", "ipfs", "s3",
    "gs", "redis", "postgres", "postgresql", "mysql", "mongodb", "amqp",
];

/// Whether `s` is a single absolute URI.
///
/// Hand-rolled rather than regex-based: this runs on every text clipping, the
/// grammar we care about is tiny, and it avoids pulling a regex engine into a
/// background daemon.
fn is_url(s: &str) -> bool {
    // A URL in a clipboard entry is a single token on a single line, and
    // nothing sane is longer than this.
    if s.is_empty() || s.len() > 4096 {
        return false;
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }

    let Some(colon) = s.find(':') else { return false };
    let (scheme, rest) = s.split_at(colon);
    let rest = &rest[1..];

    if scheme.is_empty() || !scheme.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return false;
    }
    if !scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return false;
    }
    if !URL_SCHEMES.iter().any(|k| scheme.eq_ignore_ascii_case(k)) {
        return false;
    }

    // Hierarchical schemes need an authority; opaque ones just need content.
    match scheme.to_ascii_lowercase().as_str() {
        "mailto" | "magnet" | "tel" | "sms" | "data" | "news" => !rest.is_empty(),
        _ => rest.starts_with("//") && rest.len() > 2,
    }
}

/// Parse a `text/uri-list` or `x-special/gnome-copied-files` payload.
///
/// Returns the file paths it contains. `None` when the payload is not a file
/// selection after all — dragging a hyperlink also produces a `text/uri-list`,
/// and that should be stored as a URL, not as a file (§8).
pub fn parse_uri_list(payload: &str) -> Option<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    let mut saw_non_file = false;

    for raw_line in payload.lines() {
        let line = raw_line.trim();
        // `text/uri-list` comments, and the leading `copy`/`cut` verb that
        // `x-special/gnome-copied-files` puts on its first line.
        if line.is_empty()
            || line.starts_with('#')
            || line.eq_ignore_ascii_case("copy")
            || line.eq_ignore_ascii_case("cut")
        {
            continue;
        }

        if let Some(encoded) = line.strip_prefix("file://") {
            // Strip an optional authority (`file://localhost/path`).
            let encoded = match encoded.find('/') {
                Some(0) => encoded,
                Some(idx) => &encoded[idx..],
                None => continue,
            };
            let decoded = urlencoding::decode(encoded)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| encoded.to_string());
            if !decoded.is_empty() {
                paths.push(std::path::PathBuf::from(decoded));
            }
        } else {
            saw_non_file = true;
        }
    }

    if paths.is_empty() || saw_non_file {
        None
    } else {
        Some(paths)
    }
}

/// Normalise a retrieved selection into something storable.
///
/// `blobs` is used to write image, file-list and over-large text payloads, so
/// the caller must pass a store handle usable from this thread (`BlobStore` is
/// just a path, so cloning it is free).
pub fn normalize(
    raw: &RawClip,
    settings: &Settings,
    blobs: &BlobStore,
) -> Result<std::result::Result<NewClip, Rejection>> {
    let size = raw.bytes.len() as u64;
    if let Some(limit) = settings.clipboard.max_item_bytes() {
        if size > limit {
            return Ok(Err(Rejection::TooLarge { size, limit }));
        }
    }
    if raw.bytes.is_empty() {
        return Ok(Err(Rejection::Empty));
    }

    let class = classify_target(&raw.mime);
    match class {
        TargetClass::Files if !settings.clipboard.store_files => {
            Ok(Err(Rejection::FilesDisabled))
        }
        TargetClass::Image if !settings.clipboard.store_images => {
            Ok(Err(Rejection::ImagesDisabled))
        }
        TargetClass::Files => normalize_files(raw, settings, blobs),
        TargetClass::Image => normalize_image(raw, blobs),
        TargetClass::Text => normalize_text(raw, settings, blobs),
    }
}

fn classify_target(mime: &str) -> TargetClass {
    let lower = mime.to_ascii_lowercase();
    if FILE_TARGETS.iter().any(|t| t.eq_ignore_ascii_case(mime)) {
        TargetClass::Files
    } else if lower.starts_with("image/") {
        TargetClass::Image
    } else {
        TargetClass::Text
    }
}

fn base_metadata(raw: &RawClip) -> Metadata {
    Metadata {
        source_app: raw.source_app.as_ref().and_then(|a| a.label()),
        offered_mime: raw.offered.clone(),
        selection: (raw.selection != Selection::Clipboard).then_some(raw.selection),
        ..Default::default()
    }
}

fn normalize_text(
    raw: &RawClip,
    settings: &Settings,
    blobs: &BlobStore,
) -> Result<std::result::Result<NewClip, Rejection>> {
    let text = match std::str::from_utf8(&raw.bytes) {
        Ok(t) => t,
        Err(_) => return Ok(Err(Rejection::NotUtf8)),
    };

    // A `text/uri-list` that we were handed as text may still be a file
    // selection (some applications offer it without a more specific type).
    if raw.mime.eq_ignore_ascii_case("text/uri-list") || raw.mime.starts_with("x-special/") {
        if let Some(paths) = parse_uri_list(text) {
            if settings.clipboard.store_files {
                return Ok(Ok(build_files(raw, &paths, blobs)?));
            }
            return Ok(Err(Rejection::FilesDisabled));
        }
    }

    let stored: &str = if settings.clipboard.trim_whitespace {
        text.trim()
    } else {
        text
    };
    if stored.trim().is_empty() {
        return Ok(Err(Rejection::Empty));
    }

    let kind = classify_text(stored);
    let mime = normalize_text_mime(&raw.mime);
    let content_hash =
        blobs::hash_parts(&[kind.as_str().as_bytes(), mime.as_bytes(), stored.as_bytes()]);

    let mut metadata = base_metadata(raw);
    metadata.line_count = Some(stored.lines().count().max(1) as u32);
    metadata.char_count = Some(stored.chars().count() as u32);

    // Very long text goes out of line so the database stays small (§33).
    let (text_content, blob_hash) = if stored.len() > INLINE_TEXT_LIMIT {
        (None, Some(blobs.put(stored.as_bytes())?))
    } else {
        (Some(stored.to_string()), None)
    };

    Ok(Ok(NewClip {
        kind,
        mime_type: mime,
        content_hash,
        text_content,
        blob_hash,
        preview: model::truncate_chars(stored, settings.appearance.preview_length as usize),
        metadata,
        size_bytes: stored.len() as u64,
    }))
}

/// Canonicalise the handful of equivalent text target names so that copying the
/// same string from a GTK app and a Qt app does not produce two entries.
fn normalize_text_mime(mime: &str) -> String {
    let lower = mime.to_ascii_lowercase();
    match lower.as_str() {
        "utf8_string" | "string" | "text" | "compound_text" | "text/plain" => {
            "text/plain;charset=utf-8".to_string()
        }
        _ if lower.starts_with("text/plain;charset=utf") => "text/plain;charset=utf-8".to_string(),
        _ => mime.to_string(),
    }
}

fn normalize_image(
    raw: &RawClip,
    blobs: &BlobStore,
) -> Result<std::result::Result<NewClip, Rejection>> {
    let probe = probe_image(&raw.bytes);
    let format = probe
        .as_ref()
        .map(|p| p.format.clone())
        .unwrap_or_else(|| format_from_mime(&raw.mime));

    let content_hash =
        blobs::hash_parts(&[b"image", raw.mime.to_ascii_lowercase().as_bytes(), &raw.bytes]);
    let blob_hash = blobs.put(&raw.bytes)?;

    let mut metadata = base_metadata(raw);
    metadata.image = Some(ImageMeta {
        width: probe.as_ref().map(|p| p.width).unwrap_or(0),
        height: probe.as_ref().map(|p| p.height).unwrap_or(0),
        format: format.clone(),
    });

    // The preview doubles as search text, so it carries the dimensions and
    // format rather than being a bare label (§7).
    let preview = match &probe {
        Some(p) => format!(
            "Image {} × {} {} {}",
            p.width,
            p.height,
            format,
            model::format_size(raw.bytes.len() as u64)
        ),
        None => format!(
            "Image {} {}",
            format,
            model::format_size(raw.bytes.len() as u64)
        ),
    };

    Ok(Ok(NewClip {
        kind: ClipKind::Image,
        mime_type: raw.mime.to_ascii_lowercase(),
        content_hash,
        text_content: None,
        blob_hash: Some(blob_hash),
        preview,
        metadata,
        size_bytes: raw.bytes.len() as u64,
    }))
}

fn normalize_files(
    raw: &RawClip,
    settings: &Settings,
    blobs: &BlobStore,
) -> Result<std::result::Result<NewClip, Rejection>> {
    let text = match std::str::from_utf8(&raw.bytes) {
        Ok(t) => t,
        Err(_) => return Ok(Err(Rejection::NotUtf8)),
    };
    match parse_uri_list(text) {
        Some(paths) => Ok(Ok(build_files(raw, &paths, blobs)?)),
        // A `text/uri-list` of http URLs: store it as text instead of
        // pretending it is a file selection.
        None => normalize_text(
            &RawClip { mime: "text/plain;charset=utf-8".to_string(), ..raw.clone() },
            settings,
            blobs,
        ),
    }
}

fn build_files(raw: &RawClip, paths: &[std::path::PathBuf], blobs: &BlobStore) -> Result<NewClip> {
    if paths.is_empty() {
        bail!("file selection contained no paths");
    }

    let files: Vec<FileMeta> = paths
        .iter()
        .cloned()
        .map(FileMeta::from_path)
        .collect();

    // Re-serialise to a canonical `text/uri-list`. Storing the canonical form
    // rather than the original bytes means the same selection copied from
    // Nautilus and from Dolphin de-duplicates, and it is what we re-offer on
    // restore so every file manager understands it.
    let canonical = files
        .iter()
        .map(|f| format!("file://{}", urlencoding::encode(&f.path).replace("%2F", "/")))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n";

    let content_hash = blobs::hash_parts(&[b"files", b"text/uri-list", canonical.as_bytes()]);
    let blob_hash = blobs.put(canonical.as_bytes())?;

    let names = files
        .iter()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let total: u64 = files.iter().filter_map(|f| f.size).sum();
    let preview = if files.len() == 1 {
        files[0].path.clone()
    } else {
        format!("{} files: {names}", files.len())
    };

    let mut metadata = base_metadata(raw);
    metadata.files = files;

    Ok(NewClip {
        kind: ClipKind::Files,
        mime_type: "text/uri-list".to_string(),
        content_hash,
        text_content: None,
        blob_hash: Some(blob_hash),
        preview,
        metadata,
        size_bytes: total.max(canonical.len() as u64),
    })
}

fn format_from_mime(mime: &str) -> String {
    mime.to_ascii_lowercase()
        .strip_prefix("image/")
        .map(|s| s.trim_start_matches("x-").to_string())
        .unwrap_or_else(|| "image".to_string())
}

/// Result of reading an image header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageProbe {
    pub width: u32,
    pub height: u32,
    pub format: String,
}

/// Read dimensions and format from an image's header bytes.
///
/// Header parsing rather than decoding: this runs on the monitor thread for
/// every copied image, and decoding a 50 megapixel screenshot just to put
/// "1920 × 1080" in a list row would be absurd. The UI decodes lazily, via
/// gdk-pixbuf, only for rows that are actually on screen.
pub fn probe_image(bytes: &[u8]) -> Option<ImageProbe> {
    probe_png(bytes)
        .or_else(|| probe_gif(bytes))
        .or_else(|| probe_bmp(bytes))
        .or_else(|| probe_webp(bytes))
        .or_else(|| probe_jpeg(bytes))
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le16(b: &[u8]) -> u32 {
    u16::from_le_bytes([b[0], b[1]]) as u32
}
fn be16(b: &[u8]) -> u32 {
    u16::from_be_bytes([b[0], b[1]]) as u32
}

fn probe_png(b: &[u8]) -> Option<ImageProbe> {
    // 8-byte signature, then an IHDR chunk whose data starts at offset 16.
    if b.len() < 24 || &b[..8] != b"\x89PNG\r\n\x1a\n" || &b[12..16] != b"IHDR" {
        return None;
    }
    Some(ImageProbe {
        width: be32(&b[16..20]),
        height: be32(&b[20..24]),
        format: "png".into(),
    })
}

fn probe_gif(b: &[u8]) -> Option<ImageProbe> {
    if b.len() < 10 || (&b[..6] != b"GIF87a" && &b[..6] != b"GIF89a") {
        return None;
    }
    Some(ImageProbe {
        width: le16(&b[6..8]),
        height: le16(&b[8..10]),
        format: "gif".into(),
    })
}

fn probe_bmp(b: &[u8]) -> Option<ImageProbe> {
    if b.len() < 26 || &b[..2] != b"BM" {
        return None;
    }
    // BITMAPINFOHEADER stores height as a signed value; a negative height means
    // a top-down bitmap.
    let width = le32(&b[18..22]) as i32;
    let height = le32(&b[22..26]) as i32;
    Some(ImageProbe {
        width: width.unsigned_abs(),
        height: height.unsigned_abs(),
        format: "bmp".into(),
    })
}

fn probe_webp(b: &[u8]) -> Option<ImageProbe> {
    if b.len() < 30 || &b[..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return None;
    }
    match &b[12..16] {
        // Simple lossy: 14-bit dimensions after a 3-byte sync code.
        b"VP8 " => {
            if b.len() < 30 || &b[23..26] != b"\x9d\x01\x2a" {
                return None;
            }
            Some(ImageProbe {
                width: le16(&b[26..28]) & 0x3fff,
                height: le16(&b[28..30]) & 0x3fff,
                format: "webp".into(),
            })
        }
        // Lossless: 14-bit dimensions minus one, bit-packed after a signature.
        b"VP8L" => {
            if b.len() < 25 || b[20] != 0x2f {
                return None;
            }
            let bits = le32(&b[21..25]);
            Some(ImageProbe {
                width: (bits & 0x3fff) + 1,
                height: ((bits >> 14) & 0x3fff) + 1,
                format: "webp".into(),
            })
        }
        // Extended format: 24-bit dimensions minus one.
        b"VP8X" => {
            if b.len() < 30 {
                return None;
            }
            let w = u32::from_le_bytes([b[24], b[25], b[26], 0]) + 1;
            let h = u32::from_le_bytes([b[27], b[28], b[29], 0]) + 1;
            Some(ImageProbe { width: w, height: h, format: "webp".into() })
        }
        _ => None,
    }
}

fn probe_jpeg(b: &[u8]) -> Option<ImageProbe> {
    if b.len() < 4 || b[0] != 0xff || b[1] != 0xd8 {
        return None;
    }
    let mut i = 2usize;
    // Walk the marker segments looking for a Start Of Frame.
    while i + 9 < b.len() {
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // Padding and standalone markers carry no length field.
        if marker == 0xff {
            i += 1;
            continue;
        }
        if matches!(marker, 0xd8 | 0xd9) || (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = be16(&b[i + 2..i + 4]) as usize;
        if len < 2 {
            return None;
        }
        // SOF0..SOF15 except the DHT/JPG/DAC markers interleaved in that range.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            if i + 9 >= b.len() {
                return None;
            }
            return Some(ImageProbe {
                height: be16(&b[i + 5..i + 7]),
                width: be16(&b[i + 7..i + 9]),
                format: "jpeg".into(),
            });
        }
        i += 2 + len;
    }
    // A JPEG we could not walk is still a JPEG.
    Some(ImageProbe { width: 0, height: 0, format: "jpeg".into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::default()
    }

    fn raw(mime: &str, bytes: &[u8]) -> RawClip {
        RawClip {
            offered: vec![mime.to_string()],
            mime: mime.to_string(),
            bytes: bytes.to_vec(),
            selection: Selection::Clipboard,
            source_app: None,
        }
    }

    fn blob_store() -> (tempfile::TempDir, BlobStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path().join("blobs")).unwrap();
        (tmp, store)
    }

    fn norm(raw: &RawClip, settings: &Settings) -> std::result::Result<NewClip, Rejection> {
        let (_tmp, blobs) = blob_store();
        normalize(raw, settings, &blobs).unwrap()
    }

    // -- target selection -----------------------------------------------------

    #[test]
    fn files_win_over_the_thumbnail_a_file_manager_also_offers() {
        let offered = vec![
            "image/png".to_string(),
            "text/uri-list".to_string(),
            "UTF8_STRING".to_string(),
        ];
        let chosen = choose_target(&offered, &settings()).unwrap();
        assert_eq!(chosen.class, TargetClass::Files);
        assert_eq!(chosen.mime, "text/uri-list");
    }

    #[test]
    fn png_wins_over_jpeg_and_text() {
        let offered = vec![
            "text/plain".to_string(),
            "image/jpeg".to_string(),
            "image/png".to_string(),
        ];
        let chosen = choose_target(&offered, &settings()).unwrap();
        assert_eq!(chosen.mime, "image/png");
    }

    #[test]
    fn utf8_text_wins_over_legacy_string() {
        let offered = vec![
            "STRING".to_string(),
            "TEXT".to_string(),
            "UTF8_STRING".to_string(),
            "text/plain;charset=utf-8".to_string(),
        ];
        assert_eq!(
            choose_target(&offered, &settings()).unwrap().mime,
            "text/plain;charset=utf-8"
        );
    }

    #[test]
    fn disabling_images_falls_back_to_text() {
        let mut s = settings();
        s.clipboard.store_images = false;
        let offered = vec!["image/png".to_string(), "UTF8_STRING".to_string()];
        let chosen = choose_target(&offered, &s).unwrap();
        assert_eq!(chosen.class, TargetClass::Text);
    }

    #[test]
    fn disabling_files_falls_back_to_text() {
        let mut s = settings();
        s.clipboard.store_files = false;
        let offered = vec!["text/uri-list".to_string(), "UTF8_STRING".to_string()];
        assert_eq!(
            choose_target(&offered, &s).unwrap().class,
            TargetClass::Text
        );
    }

    #[test]
    fn only_meta_targets_means_nothing_to_store() {
        let offered = vec![
            "TARGETS".to_string(),
            "TIMESTAMP".to_string(),
            "MULTIPLE".to_string(),
            "SAVE_TARGETS".to_string(),
        ];
        assert!(choose_target(&offered, &settings()).is_none());
    }

    #[test]
    fn rich_text_only_selections_are_not_stored_as_text() {
        // text/html alone is not something we can faithfully re-offer as text.
        let offered = vec!["text/html".to_string(), "TARGETS".to_string()];
        assert!(choose_target(&offered, &settings()).is_none());
    }

    #[test]
    fn an_unknown_text_subtype_is_accepted_as_a_last_resort() {
        let offered = vec!["text/x-moz-url".to_string(), "TARGETS".to_string()];
        let chosen = choose_target(&offered, &settings()).unwrap();
        assert_eq!(chosen.class, TargetClass::Text);
        assert_eq!(chosen.mime, "text/x-moz-url");
    }

    #[test]
    fn target_matching_is_case_insensitive() {
        let offered = vec!["IMAGE/PNG".to_string()];
        assert_eq!(
            choose_target(&offered, &settings()).unwrap().class,
            TargetClass::Image
        );
    }

    #[test]
    fn empty_offer_list_yields_nothing() {
        assert!(choose_target(&[], &settings()).is_none());
    }

    // -- url detection --------------------------------------------------------

    #[test]
    fn urls_are_detected() {
        for url in [
            "https://example.com",
            "https://github.com/user/repository",
            "http://localhost:8080/path?query=1#frag",
            "ftp://files.example.org/pub",
            "mailto:someone@example.com",
            "magnet:?xt=urn:btih:abc",
            "ssh://git@github.com/example/repo.git",
            "file:///home/user/file.txt",
            "srt://host:9000",
        ] {
            assert_eq!(classify_text(url), ClipKind::Url, "{url}");
        }
    }

    #[test]
    fn surrounding_whitespace_does_not_prevent_url_detection() {
        assert_eq!(classify_text("  https://example.com \n"), ClipKind::Url);
    }

    #[test]
    fn non_urls_stay_text() {
        for text in [
            "Hello World",
            "sudo systemctl restart nginx",
            "see https://example.com for details",
            "https://example.com and https://other.com",
            "example.com",
            "www.example.com",
            "http://",
            "https:",
            "://example.com",
            "C:\\Users\\file.txt",
            "key: value",
            "12:30",
            "not-a-scheme://host",
            "",
        ] {
            assert_eq!(classify_text(text), ClipKind::Text, "{text:?}");
        }
    }

    #[test]
    fn a_multiline_block_is_never_a_url() {
        assert_eq!(classify_text("https://example.com\nhttps://b.com"), ClipKind::Text);
    }

    // -- uri lists ------------------------------------------------------------

    #[test]
    fn uri_list_is_parsed_and_percent_decoded() {
        let payload = "file:///home/user/Documents/report.pdf\r\n\
                       file:///home/user/My%20Videos/video%20file.mp4\r\n";
        let paths = parse_uri_list(payload).unwrap();
        assert_eq!(
            paths,
            vec![
                std::path::PathBuf::from("/home/user/Documents/report.pdf"),
                std::path::PathBuf::from("/home/user/My Videos/video file.mp4"),
            ]
        );
    }

    #[test]
    fn gnome_copied_files_verb_is_skipped() {
        let payload = "copy\nfile:///tmp/a.txt\nfile:///tmp/b.txt";
        assert_eq!(parse_uri_list(payload).unwrap().len(), 2);
        let cut = "cut\nfile:///tmp/a.txt";
        assert_eq!(parse_uri_list(cut).unwrap().len(), 1);
    }

    #[test]
    fn uri_list_comments_are_skipped() {
        let payload = "# a comment\nfile:///tmp/a.txt\n\n";
        assert_eq!(parse_uri_list(payload).unwrap().len(), 1);
    }

    #[test]
    fn file_authority_is_stripped() {
        let paths = parse_uri_list("file://localhost/tmp/a.txt").unwrap();
        assert_eq!(paths, vec![std::path::PathBuf::from("/tmp/a.txt")]);
    }

    #[test]
    fn a_link_offered_as_a_uri_list_is_not_a_file_selection() {
        assert!(parse_uri_list("https://example.com/page").is_none());
        // Mixed lists are rejected too: we cannot represent them as files.
        assert!(parse_uri_list("file:///tmp/a.txt\nhttps://example.com").is_none());
        assert!(parse_uri_list("").is_none());
        assert!(parse_uri_list("copy\n").is_none());
    }

    #[test]
    fn a_uri_list_of_links_is_stored_as_text() {
        let clip = norm(&raw("text/uri-list", b"https://example.com\r\n"), &settings()).unwrap();
        assert_eq!(clip.kind, ClipKind::Url);
    }

    #[test]
    fn file_selections_are_normalised_and_described() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("report.pdf");
        std::fs::write(&file, vec![0u8; 1234]).unwrap();
        let payload = format!("file://{}\r\n", file.display());

        let (_t, blobs) = blob_store();
        let clip = normalize(&raw("text/uri-list", payload.as_bytes()), &settings(), &blobs)
            .unwrap()
            .unwrap();

        assert_eq!(clip.kind, ClipKind::Files);
        assert_eq!(clip.mime_type, "text/uri-list");
        assert_eq!(clip.metadata.files.len(), 1);
        assert_eq!(clip.metadata.files[0].name, "report.pdf");
        assert_eq!(clip.metadata.files[0].size, Some(1234));
        assert!(clip.blob_hash.is_some());
        // The canonical payload we will re-offer.
        let stored = blobs.get(clip.blob_hash.as_ref().unwrap()).unwrap();
        assert_eq!(
            String::from_utf8(stored).unwrap(),
            format!("file://{}\r\n", file.display())
        );
    }

    #[test]
    fn the_same_selection_from_two_file_managers_deduplicates() {
        let (_t, blobs) = blob_store();
        let nautilus = raw(
            "x-special/gnome-copied-files",
            b"copy\nfile:///tmp/a.txt\nfile:///tmp/b.txt",
        );
        let plain = raw("text/uri-list", b"file:///tmp/a.txt\r\nfile:///tmp/b.txt\r\n");

        let a = normalize(&nautilus, &settings(), &blobs).unwrap().unwrap();
        let b = normalize(&plain, &settings(), &blobs).unwrap().unwrap();
        assert_eq!(a.content_hash, b.content_hash);
    }

    #[test]
    fn missing_files_still_produce_an_entry() {
        let clip = norm(
            &raw("text/uri-list", b"file:///does/not/exist.txt\r\n"),
            &settings(),
        )
        .unwrap();
        assert_eq!(clip.metadata.files[0].size, None);
        assert_eq!(clip.metadata.files[0].name, "exist.txt");
    }

    // -- text -----------------------------------------------------------------

    #[test]
    fn plain_text_is_preserved_exactly() {
        let clip = norm(&raw("UTF8_STRING", b"Hello World"), &settings()).unwrap();
        assert_eq!(clip.kind, ClipKind::Text);
        assert_eq!(clip.text_content.as_deref(), Some("Hello World"));
        assert_eq!(clip.mime_type, "text/plain;charset=utf-8");
    }

    #[test]
    fn whitespace_structure_is_preserved() {
        let payload = "line 1\n\tindented\n  spaced  \n\nlast";
        let clip = norm(&raw("text/plain", payload.as_bytes()), &settings()).unwrap();
        assert_eq!(clip.text_content.as_deref(), Some(payload));
        assert_eq!(clip.metadata.line_count, Some(5));
    }

    #[test]
    fn unicode_and_emoji_survive() {
        let payload = "日本語 Ελληνικά Русский العربية 👍🏽🎉 café";
        let clip = norm(&raw("text/plain;charset=utf-8", payload.as_bytes()), &settings()).unwrap();
        assert_eq!(clip.text_content.as_deref(), Some(payload));
        assert_eq!(clip.metadata.char_count, Some(payload.chars().count() as u32));
    }

    #[test]
    fn trim_whitespace_is_opt_in() {
        let payload = b"  padded  \n";
        assert_eq!(
            norm(&raw("text/plain", payload), &settings())
                .unwrap()
                .text_content
                .as_deref(),
            Some("  padded  \n")
        );

        let mut s = settings();
        s.clipboard.trim_whitespace = true;
        assert_eq!(
            norm(&raw("text/plain", payload), &s).unwrap().text_content.as_deref(),
            Some("padded")
        );
    }

    #[test]
    fn equivalent_text_targets_produce_one_entry() {
        let (_t, blobs) = blob_store();
        let hashes: Vec<_> = ["UTF8_STRING", "STRING", "text/plain", "text/plain;charset=UTF-8"]
            .iter()
            .map(|m| {
                normalize(&raw(m, b"same text"), &settings(), &blobs)
                    .unwrap()
                    .unwrap()
                    .content_hash
            })
            .collect();
        assert!(
            hashes.windows(2).all(|w| w[0] == w[1]),
            "equivalent targets must hash alike: {hashes:?}"
        );
    }

    #[test]
    fn invalid_utf8_claiming_to_be_text_is_rejected() {
        let err = norm(&raw("text/plain", &[0xff, 0xfe, 0x00, 0x01]), &settings()).unwrap_err();
        assert_eq!(err, Rejection::NotUtf8);
    }

    #[test]
    fn empty_and_whitespace_only_selections_are_rejected() {
        assert_eq!(
            norm(&raw("text/plain", b""), &settings()).unwrap_err(),
            Rejection::Empty
        );
        assert_eq!(
            norm(&raw("text/plain", b"   \n\t  "), &settings()).unwrap_err(),
            Rejection::Empty
        );
    }

    #[test]
    fn preview_is_truncated_without_splitting_characters() {
        let mut s = settings();
        s.appearance.preview_length = 10;
        let clip = norm(&raw("text/plain", "日本語のテキストはとても長い".as_bytes()), &s).unwrap();
        assert_eq!(clip.preview.chars().count(), 11, "10 chars plus the ellipsis");
        assert!(clip.preview.ends_with('…'));
    }

    #[test]
    fn very_long_text_goes_to_the_blob_store() {
        let (_t, blobs) = blob_store();
        let huge = "x".repeat(INLINE_TEXT_LIMIT + 1);
        let clip = normalize(&raw("text/plain", huge.as_bytes()), &settings(), &blobs)
            .unwrap()
            .unwrap();
        assert!(clip.text_content.is_none(), "should not be inline");
        let hash = clip.blob_hash.as_ref().unwrap();
        assert_eq!(blobs.get(hash).unwrap().len(), huge.len());
        assert_eq!(clip.kind, ClipKind::Text);
    }

    // -- size limits ----------------------------------------------------------

    #[test]
    fn oversized_items_are_rejected_with_a_user_message() {
        let mut s = settings();
        s.clipboard.max_item_size_mb = 1;
        let payload = vec![b'a'; 2 * 1024 * 1024];
        let err = norm(&raw("text/plain", &payload), &s).unwrap_err();
        match err {
            Rejection::TooLarge { size, limit } => {
                assert_eq!(size, 2 * 1024 * 1024);
                assert_eq!(limit, 1024 * 1024);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(err.user_message().unwrap().contains("exceeds"));
    }

    #[test]
    fn unlimited_size_accepts_a_large_item() {
        let mut s = settings();
        s.clipboard.max_item_size_mb = 0;
        let payload = vec![b'a'; 4 * 1024 * 1024];
        assert!(norm(&raw("text/plain", &payload), &s).is_ok());
    }

    #[test]
    fn disabled_kinds_are_rejected_at_normalisation_too() {
        let mut s = settings();
        s.clipboard.store_images = false;
        assert_eq!(
            norm(&raw("image/png", b"\x89PNG\r\n\x1a\n"), &s).unwrap_err(),
            Rejection::ImagesDisabled
        );

        let mut s = settings();
        s.clipboard.store_files = false;
        assert_eq!(
            norm(&raw("text/uri-list", b"file:///tmp/a"), &s).unwrap_err(),
            Rejection::FilesDisabled
        );
    }

    #[test]
    fn routine_rejections_do_not_notify_the_user() {
        // §34: only the actionable case gets a notification.
        assert!(Rejection::Empty.user_message().is_none());
        assert!(Rejection::NotUtf8.user_message().is_none());
        assert!(Rejection::ImagesDisabled.user_message().is_none());
        assert!(Rejection::TooLarge { size: 2, limit: 1 }.user_message().is_some());
    }

    // -- images ---------------------------------------------------------------

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    #[test]
    fn png_dimensions_are_read_from_the_header() {
        let probe = probe_image(&png(1920, 1080)).unwrap();
        assert_eq!((probe.width, probe.height, probe.format.as_str()), (1920, 1080, "png"));
    }

    #[test]
    fn gif_dimensions_are_read() {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&640u16.to_le_bytes());
        v.extend_from_slice(&480u16.to_le_bytes());
        let probe = probe_image(&v).unwrap();
        assert_eq!((probe.width, probe.height, probe.format.as_str()), (640, 480, "gif"));
    }

    #[test]
    fn bmp_dimensions_including_top_down_are_read() {
        let mut v = b"BM".to_vec();
        v.extend_from_slice(&[0u8; 16]);
        v.extend_from_slice(&100i32.to_le_bytes());
        v.extend_from_slice(&(-50i32).to_le_bytes());
        let probe = probe_image(&v).unwrap();
        assert_eq!((probe.width, probe.height), (100, 50));
        assert_eq!(probe.format, "bmp");
    }

    #[test]
    fn webp_lossy_lossless_and_extended_are_read() {
        // VP8 (lossy)
        let mut lossy = b"RIFF\x00\x00\x00\x00WEBPVP8 ".to_vec();
        lossy.extend_from_slice(&[0u8; 7]);
        lossy.extend_from_slice(b"\x9d\x01\x2a");
        lossy.extend_from_slice(&(320u16).to_le_bytes());
        lossy.extend_from_slice(&(240u16).to_le_bytes());
        let probe = probe_image(&lossy).unwrap();
        assert_eq!((probe.width, probe.height, probe.format.as_str()), (320, 240, "webp"));

        // VP8L (lossless): width-1 in bits 0..14, height-1 in bits 14..28.
        let mut lossless = b"RIFF\x00\x00\x00\x00WEBPVP8L".to_vec();
        lossless.extend_from_slice(&[0u8; 4]);
        lossless.push(0x2f);
        let bits: u32 = (99u32) | (199u32 << 14);
        lossless.extend_from_slice(&bits.to_le_bytes());
        lossless.extend_from_slice(&[0u8; 8]);
        let probe = probe_image(&lossless).unwrap();
        assert_eq!((probe.width, probe.height), (100, 200));

        // VP8X (extended): 24-bit width-1 and height-1.
        let mut extended = b"RIFF\x00\x00\x00\x00WEBPVP8X".to_vec();
        extended.extend_from_slice(&[0u8; 8]);
        extended.extend_from_slice(&[0x3f, 0x00, 0x00]); // 63 -> 64
        extended.extend_from_slice(&[0x7f, 0x00, 0x00]); // 127 -> 128
        let probe = probe_image(&extended).unwrap();
        assert_eq!((probe.width, probe.height), (64, 128));
    }

    #[test]
    fn jpeg_dimensions_are_found_past_the_exif_segment() {
        let mut v = vec![0xff, 0xd8];
        // An APP1/Exif segment that must be skipped.
        v.extend_from_slice(&[0xff, 0xe1]);
        v.extend_from_slice(&(2u16 + 20).to_be_bytes());
        v.extend_from_slice(&[0u8; 20]);
        // SOF0.
        v.extend_from_slice(&[0xff, 0xc0]);
        v.extend_from_slice(&(17u16).to_be_bytes());
        v.push(8);
        v.extend_from_slice(&(768u16).to_be_bytes()); // height
        v.extend_from_slice(&(1024u16).to_be_bytes()); // width
        v.extend_from_slice(&[0u8; 10]);

        let probe = probe_image(&v).unwrap();
        assert_eq!((probe.width, probe.height, probe.format.as_str()), (1024, 768, "jpeg"));
    }

    #[test]
    fn a_truncated_or_unknown_image_does_not_panic() {
        for bytes in [
            &b"\x89PNG"[..],
            &b"BM"[..],
            &b"RIFF"[..],
            &b"\xff\xd8"[..],
            &b"GIF8"[..],
            &[][..],
            &[0u8; 3][..],
        ] {
            let _ = probe_image(bytes);
        }
    }

    #[test]
    fn an_image_clip_carries_dimensions_and_searchable_preview() {
        let (_t, blobs) = blob_store();
        let bytes = png(1920, 1080);
        let clip = normalize(&raw("image/png", &bytes), &settings(), &blobs)
            .unwrap()
            .unwrap();

        assert_eq!(clip.kind, ClipKind::Image);
        assert_eq!(clip.mime_type, "image/png");
        assert!(clip.text_content.is_none());
        let meta = clip.metadata.image.as_ref().unwrap();
        assert_eq!((meta.width, meta.height, meta.format.as_str()), (1920, 1080, "png"));
        assert!(clip.preview.contains("1920"));
        assert!(clip.preview.contains("png"));
        assert_eq!(blobs.get(clip.blob_hash.as_ref().unwrap()).unwrap(), bytes);
    }

    #[test]
    fn an_unprobeable_image_is_still_stored() {
        let (_t, blobs) = blob_store();
        let clip = normalize(&raw("image/tiff", b"II*\x00 not really a tiff"), &settings(), &blobs)
            .unwrap()
            .unwrap();
        assert_eq!(clip.kind, ClipKind::Image);
        assert_eq!(clip.metadata.image.as_ref().unwrap().format, "tiff");
        assert_eq!(clip.metadata.image.as_ref().unwrap().width, 0);
    }

    #[test]
    fn identical_images_in_different_formats_are_separate_entries() {
        let (_t, blobs) = blob_store();
        let bytes = png(10, 10);
        let a = normalize(&raw("image/png", &bytes), &settings(), &blobs).unwrap().unwrap();
        let b = normalize(&raw("image/webp", &bytes), &settings(), &blobs).unwrap().unwrap();
        assert_ne!(a.content_hash, b.content_hash, "MIME type is part of identity");
        assert_eq!(a.blob_hash, b.blob_hash, "but the payload is shared");
    }

    // -- metadata -------------------------------------------------------------

    #[test]
    fn offered_targets_and_source_app_are_recorded() {
        let mut r = raw("text/plain", b"hello");
        r.offered = vec!["TARGETS".into(), "text/plain".into(), "UTF8_STRING".into()];
        r.source_app = Some(model::AppIdentity {
            name: Some("gnome-terminal".into()),
            ..Default::default()
        });
        let clip = norm(&r, &settings()).unwrap();
        assert_eq!(clip.metadata.source_app.as_deref(), Some("gnome-terminal"));
        assert_eq!(clip.metadata.offered_mime.len(), 3);
    }

    #[test]
    fn primary_selection_is_tagged_but_clipboard_is_not() {
        let mut r = raw("text/plain", b"hello");
        r.selection = Selection::Primary;
        assert_eq!(norm(&r, &settings()).unwrap().metadata.selection, Some(Selection::Primary));

        let r = raw("text/plain", b"hello");
        assert_eq!(norm(&r, &settings()).unwrap().metadata.selection, None);
    }

    #[test]
    fn meta_targets_are_recognised() {
        assert!(is_meta_target("TARGETS"));
        assert!(is_meta_target("timestamp"));
        assert!(is_meta_target("_NET_WM_NAME"));
        assert!(is_meta_target("_GTK_TEXT_BUFFER_CONTENTS"));
        assert!(!is_meta_target("text/plain"));
        assert!(!is_meta_target("image/png"));
    }
}
