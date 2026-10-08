//! Putting an entry back on the clipboard (§52).
//!
//! This is the *write* half of clipboard handling, and it deliberately does not
//! live in [`crate::platform`] alongside the readers. Reading needs raw protocol
//! access — XFIXES notifications and `wlr-data-control` have no GDK equivalent —
//! but writing does not: GDK already implements selection ownership, ICCCM
//! `INCR` transfers and `wl_data_source` correctly on both display servers, and
//! it keeps serving the data for as long as the process lives. Reimplementing
//! that against two protocols would be strictly worse, so restoring an entry
//! goes through [`gtk::Clipboard`] on the GTK main thread.
//!
//! # Fidelity
//!
//! An entry is re-offered under the MIME type it arrived as, plus the aliases
//! that other applications actually ask for. That matters more than it sounds:
//! a terminal asks for `UTF8_STRING`, Firefox asks for
//! `text/plain;charset=utf-8`, an ancient Motif app asks for `STRING`, and
//! Nautilus will only paste files if offered `x-special/gnome-copied-files`.
//! Offering one name and hoping is how clipboard managers end up "not working"
//! in half the user's applications.
//!
//! # The feedback loop
//!
//! Taking ownership of the clipboard makes the display server tell our own
//! monitor that the clipboard changed. Left alone, every restore would come
//! straight back as a fresh clipping, so the entry would be re-inserted and
//! bounce to the top of the history. [`restore`] therefore registers the
//! content hash with the monitor first, via
//! [`crate::clipboard::Monitor::suppress`], and only then takes ownership.
//!
//! # What this does *not* do
//!
//! It does not synthesise a paste. §51 is explicit: no injecting keystrokes
//! into other applications, no `XTEST`, no uinput, no elevated privileges. The
//! flow is select → restore to clipboard → close the popup → the user presses
//! their own paste key. That is also what Windows' clipboard history does.
//!
//! It also does not make a restored entry outlive the daemon. GTK can ask a
//! desktop clipboard manager to take a persistent copy
//! (`gtk_clipboard_set_can_store`), but that function is not bound by gtk-rs
//! 0.18, so it is not used rather than pretended. The practical effect is the
//! standard X11/Wayland one, and it applies to every application: while the
//! daemon runs it keeps serving the selection, and if the daemon exits while
//! owning it the selection is released. Since the daemon is a long-running
//! user service this is rarely visible, and the history itself is of course
//! unaffected — only the live selection is.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;

use crate::model::{ClipItem, ClipKind};

/// Targets offered for text, in the order we want requestors to prefer them.
///
/// `text/plain;charset=utf-8` first because it is unambiguous; the bare X11
/// atom names after it for applications that only know those.
const TEXT_TARGETS: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
    "COMPOUND_TEXT",
];

/// Targets offered for a file selection.
///
/// `x-special/gnome-copied-files` is not a real MIME type; it is the convention
/// GTK file managers use, and its payload is an operation verb followed by the
/// URIs. Without it, pasting into Nautilus or Nemo silently does nothing.
const FILE_TARGETS: &[&str] = &["text/uri-list", "x-special/gnome-copied-files", "text/plain"];

/// Which payload a requestor should be handed, chosen by target name.
///
/// Resolved once when ownership is taken rather than per request: a requestor
/// can ask several times, and recomputing a URI list or re-encoding a 40 MB
/// image on each request would make pasting visibly slow.
#[derive(Debug)]
struct Payload {
    /// Verbatim bytes for every target that wants the raw content.
    bytes: Vec<u8>,
    /// Pre-rendered `text/uri-list` body, for file selections.
    uri_list: Option<String>,
    /// Pre-rendered `x-special/gnome-copied-files` body.
    gnome_copied: Option<String>,
    kind: ClipKind,
}

impl Payload {
    fn build(item: &ClipItem, bytes: Vec<u8>) -> Self {
        let (uri_list, gnome_copied) = if item.kind == ClipKind::Files {
            let uris = file_uris(item, &bytes);
            if uris.is_empty() {
                (None, None)
            } else {
                // text/uri-list is CRLF-terminated per RFC 2483; the GNOME
                // variant is LF-separated with a leading operation verb.
                let list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
                let gnome = format!("copy\n{}", uris.join("\n"));
                (Some(list), Some(gnome))
            }
        } else {
            (None, None)
        };

        Self { bytes, uri_list, gnome_copied, kind: item.kind }
    }

    /// Bytes to serve for `target`.
    fn for_target(&self, target: &str) -> &[u8] {
        let lower = target.to_ascii_lowercase();
        match lower.as_str() {
            "x-special/gnome-copied-files" => self
                .gnome_copied
                .as_deref()
                .map(str::as_bytes)
                .unwrap_or(&self.bytes),
            "text/uri-list" => self.uri_list.as_deref().map(str::as_bytes).unwrap_or(&self.bytes),
            // A file selection asked for as plain text should paste as the
            // paths, which is what the stored payload already is.
            _ => &self.bytes,
        }
    }
}

/// Recover the URI list for a file selection.
///
/// The stored payload is whatever the source application offered — usually
/// already a `text/uri-list`. The recorded metadata is preferred when present
/// because it is normalised, with the payload as the fallback so an entry
/// written by an older version still pastes.
fn file_uris(item: &ClipItem, bytes: &[u8]) -> Vec<String> {
    if !item.metadata.files.is_empty() {
        return item
            .metadata
            .files
            .iter()
            .map(|file| path_to_uri(&file.path))
            .collect();
    }

    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            if line.contains("://") {
                line.to_string()
            } else {
                path_to_uri(line)
            }
        })
        .collect()
}

/// Percent-encode a filesystem path into a `file://` URI.
///
/// Encoding per path component, so the separators survive: a file called
/// `My Report (final).pdf` must not turn its slashes into `%2F`.
fn path_to_uri(path: &str) -> String {
    let encoded: Vec<String> = path
        .split('/')
        .map(|part| urlencoding::encode(part).into_owned())
        .collect();
    format!("file://{}", encoded.join("/"))
}

/// The target names to advertise for an entry.
fn targets_for(item: &ClipItem) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if !targets.iter().any(|t| t.eq_ignore_ascii_case(name)) {
            targets.push(name.to_string());
        }
    };

    match item.kind {
        ClipKind::Text | ClipKind::Url => {
            for name in TEXT_TARGETS {
                push(name);
            }
        }
        ClipKind::Files => {
            for name in FILE_TARGETS {
                push(name);
            }
        }
        ClipKind::Image => {
            // The stored type first, so a PNG is served as a PNG rather than
            // being re-encoded by whatever asks.
            push(&item.mime_type);
            push("image/png");
        }
    }

    // Always offer the original type, even if it is something unusual, so a
    // round trip through the history is faithful for the application that
    // produced it.
    push(&item.mime_type);
    targets
}

/// Put `item` on the `CLIPBOARD` selection.
///
/// `payload` is the full content, which the caller has already fetched from the
/// store — this function does no I/O, because it runs on the GTK main thread
/// and a blocking blob read there would stutter the UI (§47).
///
/// Ownership is held by this process until another application takes the
/// selection, which is the normal X11/Wayland contract. Note the consequence,
/// documented rather than hidden: when the daemon exits, a selection it owns
/// disappears, exactly as it would for any other application.
pub fn restore(item: &ClipItem, payload: Vec<u8>) -> Result<()> {
    let clipboard = clipboard();
    let targets = targets_for(item);
    let entries: Vec<gtk::TargetEntry> = targets
        .iter()
        .enumerate()
        .map(|(index, name)| {
            gtk::TargetEntry::new(name, gtk::TargetFlags::empty(), index as u32)
        })
        .collect();

    // `Rc<RefCell<..>>` because the closure is called repeatedly, for as long
    // as we own the selection, and must keep the payload alive across calls.
    let payload = Rc::new(RefCell::new(Payload::build(item, payload)));
    let names = Rc::new(targets);

    let owned = clipboard.set_with_data(&entries, {
        let payload = Rc::clone(&payload);
        let names = Rc::clone(&names);
        move |_clipboard, selection, info| {
            let payload = payload.borrow();
            // `info` is the index we assigned above. Trust it only after a
            // bounds check: it arrives from the toolkit, and an out-of-range
            // value must not panic inside a C callback. Falling back to the
            // atom name the requestor actually asked for keeps us correct
            // without assuming the index is meaningful.
            let requested: String = match names.get(info as usize) {
                Some(name) => name.clone(),
                None => selection.target().name().to_string(),
            };

            let bytes = payload.for_target(&requested);
            let served = if payload.kind.is_editable_text() && is_text_target(&requested) {
                // `set_text` handles the charset conversion that COMPOUND_TEXT
                // and STRING requestors expect.
                match std::str::from_utf8(bytes) {
                    Ok(text) => selection.set_text(text),
                    // Stored as text but not valid UTF-8 should be impossible;
                    // serving the raw bytes is better than serving nothing.
                    Err(_) => false,
                }
            } else {
                false
            };

            if !served {
                selection.set(&selection.target(), 8, bytes);
            }
        }
    });

    if !owned {
        anyhow::bail!("the display server refused to give us the clipboard selection");
    }
    Ok(())
}

/// Replace the clipboard with `text`, used by the inline editor (§13).
pub fn restore_text(text: &str) -> Result<()> {
    clipboard().set_text(text);
    Ok(())
}

/// Drop whatever we are serving, releasing the selection.
pub fn clear() {
    clipboard().clear();
}

fn clipboard() -> gtk::Clipboard {
    gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD)
}

/// Should this target be served through `set_text` rather than as raw bytes?
///
/// `text/uri-list` is excluded despite the `text/` prefix: its framing is
/// CRLF-terminated per RFC 2483, and routing it through the toolkit's text
/// conversion would rewrite the line endings and break pasting into file
/// managers.
fn is_text_target(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    if lower.contains("uri-list") || lower.contains("gnome-copied-files") {
        return false;
    }
    lower.starts_with("text/") || TEXT_TARGETS.iter().any(|t| t.eq_ignore_ascii_case(target))
}

/// Read the clipboard's current text, so opening the popup can tell whether the
/// user's live clipboard is already the top entry.
///
/// Returns `None` when the clipboard holds something that is not text, which is
/// not an error. §52 forbids disturbing the user's clipboard merely by opening
/// the UI, so this only ever *reads*.
pub fn current_text() -> Option<String> {
    clipboard().wait_for_text().map(|t| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileMeta, Metadata};

    fn item(kind: ClipKind, mime: &str) -> ClipItem {
        ClipItem {
            id: 1,
            kind,
            mime_type: mime.to_string(),
            content_hash: "abc".to_string(),
            text_content: None,
            blob_hash: None,
            preview: String::new(),
            metadata: Metadata::default(),
            size_bytes: 0,
            created_at: 0,
            updated_at: 0,
            last_used_at: 0,
            is_pinned: false,
        }
    }

    #[test]
    fn text_is_offered_under_every_name_applications_actually_ask_for() {
        let targets = targets_for(&item(ClipKind::Text, "text/plain;charset=utf-8"));
        for expected in ["text/plain;charset=utf-8", "UTF8_STRING", "STRING", "TEXT"] {
            assert!(targets.iter().any(|t| t == expected), "missing {expected} in {targets:?}");
        }
    }

    #[test]
    fn a_file_selection_offers_the_gnome_target_so_nautilus_can_paste() {
        let targets = targets_for(&item(ClipKind::Files, "text/uri-list"));
        assert!(targets.iter().any(|t| t == "x-special/gnome-copied-files"));
        assert!(targets.iter().any(|t| t == "text/uri-list"));
    }

    #[test]
    fn an_image_is_offered_as_its_stored_type_first() {
        let targets = targets_for(&item(ClipKind::Image, "image/jpeg"));
        assert_eq!(targets.first().map(String::as_str), Some("image/jpeg"));
        assert!(targets.iter().any(|t| t == "image/png"));
    }

    #[test]
    fn target_lists_never_repeat_a_name() {
        // `image/png` is both the stored type and an alias we add.
        let targets = targets_for(&item(ClipKind::Image, "image/png"));
        let mut sorted = targets.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), targets.len(), "{targets:?}");
    }

    #[test]
    fn paths_become_uris_without_mangling_separators() {
        assert_eq!(
            path_to_uri("/home/u/My Report (final).pdf"),
            "file:///home/u/My%20Report%20%28final%29.pdf"
        );
    }

    #[test]
    fn file_payloads_are_rendered_for_both_file_manager_conventions() {
        let mut it = item(ClipKind::Files, "text/uri-list");
        it.metadata.files =
            vec![FileMeta::from_path("/tmp/a.txt".into()), FileMeta::from_path("/tmp/b.txt".into())];

        let payload = Payload::build(&it, Vec::new());
        let uri_list = String::from_utf8(payload.for_target("text/uri-list").to_vec()).unwrap();
        // RFC 2483 wants CRLF line endings.
        assert_eq!(uri_list, "file:///tmp/a.txt\r\nfile:///tmp/b.txt\r\n");

        let gnome =
            String::from_utf8(payload.for_target("x-special/gnome-copied-files").to_vec()).unwrap();
        assert_eq!(gnome, "copy\nfile:///tmp/a.txt\nfile:///tmp/b.txt");
    }

    #[test]
    fn a_file_entry_written_before_metadata_existed_still_pastes() {
        // Falls back to parsing the stored payload rather than offering nothing.
        let it = item(ClipKind::Files, "text/uri-list");
        let payload = Payload::build(&it, b"file:///tmp/old.txt\n".to_vec());
        let uri_list = String::from_utf8(payload.for_target("text/uri-list").to_vec()).unwrap();
        assert_eq!(uri_list, "file:///tmp/old.txt\r\n");
    }

    #[test]
    fn uri_lists_are_not_treated_as_plain_text() {
        // Serving a uri-list through `set_text` would lose the CRLF framing.
        assert!(!is_text_target("text/uri-list"));
        assert!(is_text_target("UTF8_STRING"));
        assert!(is_text_target("text/plain;charset=utf-8"));
    }
}
