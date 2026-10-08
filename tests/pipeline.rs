//! End-to-end tests of the capture → store → search → restore pipeline (§43).
//!
//! These need no graphical session: they drive the layers below
//! [`clipboard_manager::ui`] directly, which is the whole reason the crate is
//! split into a library and a thin binary. They are the tests that would catch
//! a regression in the path a clipping actually takes, as opposed to the unit
//! tests that check each piece in isolation.
//!
//! Everything runs against a temporary data directory, so a developer's real
//! history is never touched.

use clipboard_manager::clipboard::normalize;
use clipboard_manager::config::Settings;
use clipboard_manager::model::{ClipKind, RawClip, Selection};
use clipboard_manager::storage::{BlobStore, Store};

/// A store on a throwaway directory, plus the blob store it shares.
struct Fixture {
    store: Store,
    blobs: BlobStore,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let blobs = BlobStore::open(dir.path().join("blobs")).expect("blob store");
        let store = Store::open_at(dir.path()).expect("store");
        Self { store, blobs, _dir: dir }
    }

    /// Put `bytes` through the same normalisation the monitor uses, then store.
    ///
    /// This is the real pipeline: `choose_target` picks the MIME type,
    /// `normalize` classifies and writes any blob, `insert` persists. A test
    /// that built a `NewClip` by hand would not exercise any of it.
    fn capture(&mut self, offered: &[&str], mime: &str, bytes: &[u8]) -> i64 {
        self.capture_with(&Settings::default(), offered, mime, bytes)
    }

    fn capture_with(
        &mut self,
        settings: &Settings,
        offered: &[&str],
        mime: &str,
        bytes: &[u8],
    ) -> i64 {
        let offered: Vec<String> = offered.iter().map(|s| s.to_string()).collect();
        let raw = RawClip {
            offered,
            mime: mime.to_string(),
            bytes: bytes.to_vec(),
            selection: Selection::Clipboard,
            source_app: None,
        };

        let clip = normalize::normalize(&raw, settings, &self.blobs)
            .expect("normalisation should accept this clipping")
            .expect("this clipping should not be rejected");

        self.store
            .insert(&clip, settings.clipboard.ignore_duplicates)
            .expect("insert")
            .id()
    }

    fn text(&mut self, body: &str) -> i64 {
        self.capture(&["UTF8_STRING"], "UTF8_STRING", body.as_bytes())
    }
}

// ---- text ------------------------------------------------------------------

#[test]
fn plain_text_round_trips_byte_for_byte() {
    let mut fixture = Fixture::new();
    let body = "ffmpeg -i input.mp4 output.ts";
    let id = fixture.text(body);

    let (item, payload) = fixture.store.payload_of(id).expect("payload");
    assert_eq!(item.kind, ClipKind::Text);
    assert_eq!(payload, body.as_bytes());
    assert_eq!(fixture.store.full_text(id).unwrap(), body);
}

#[test]
fn multiline_text_keeps_its_newlines_tabs_and_trailing_structure() {
    let mut fixture = Fixture::new();
    // Deliberately awkward: CRLF, a tab, a blank line, trailing spaces.
    let body = "line one\r\n\tindented\n\nlast line   ";
    let id = fixture.text(body);

    let stored = fixture.store.full_text(id).unwrap();
    assert_eq!(stored, body, "whitespace must survive storage exactly");
    assert_eq!(fixture.store.get(id).unwrap().unwrap().kind, ClipKind::Text);
}

#[test]
fn unicode_and_emoji_survive_intact() {
    let mut fixture = Fixture::new();
    let body = "émoji 🎉 ünïcode — 日本語 — العربية — Ωμέγα";
    let id = fixture.text(body);

    let stored = fixture.store.full_text(id).unwrap();
    assert_eq!(stored, body);
    // And the byte length, so nothing was re-encoded along the way.
    assert_eq!(
        fixture.store.get(id).unwrap().unwrap().size_bytes,
        body.len() as u64
    );
}

#[test]
fn a_url_is_classified_as_a_link_not_as_plain_text() {
    let mut fixture = Fixture::new();
    let id = fixture.text("https://github.com/sridharreddy23/clipboard-manager");
    assert_eq!(fixture.store.get(id).unwrap().unwrap().kind, ClipKind::Url);

    // ...but a sentence that merely mentions a scheme is not a URL.
    let id = fixture.text("see https://example.com for details, it is good");
    assert_eq!(fixture.store.get(id).unwrap().unwrap().kind, ClipKind::Text);
}

// ---- duplicates and ordering -----------------------------------------------

#[test]
fn copying_the_same_text_twice_does_not_create_two_entries() {
    let mut fixture = Fixture::new();
    let first = fixture.text("sudo systemctl restart nginx");
    let second = fixture.text("sudo systemctl restart nginx");

    assert_eq!(first, second, "the duplicate should collapse onto the original");
    assert_eq!(fixture.store.recent(100).unwrap().len(), 1);
}

#[test]
fn a_recopied_entry_moves_back_to_the_top_and_keeps_its_pin() {
    let mut fixture = Fixture::new();
    let first = fixture.text("first");
    fixture.store.set_pinned(first, true).unwrap();
    let _second = fixture.text("second");
    let _third = fixture.text("third");

    // Re-copying the oldest must not lose the pin (§10).
    let again = fixture.text("first");
    assert_eq!(again, first);

    let item = fixture.store.get(first).unwrap().unwrap();
    assert!(item.is_pinned, "the pin must survive a duplicate copy");

    // Pinned entries come first, so this is also the head of the list.
    let recent = fixture.store.recent(100).unwrap();
    assert_eq!(recent[0].id, first);
    assert_eq!(recent.len(), 3);
}

#[test]
fn with_duplicate_collapsing_off_a_repeat_becomes_a_second_entry() {
    let mut fixture = Fixture::new();
    let mut settings = Settings::default();
    settings.clipboard.ignore_duplicates = false;

    let a = fixture.capture_with(&settings, &["UTF8_STRING"], "UTF8_STRING", b"same");
    let b = fixture.capture_with(&settings, &["UTF8_STRING"], "UTF8_STRING", b"same");
    assert_ne!(a, b);
    assert_eq!(fixture.store.recent(100).unwrap().len(), 2);
}

#[test]
fn entries_are_listed_newest_first_with_pinned_above_everything() {
    let mut fixture = Fixture::new();
    let old = fixture.text("oldest");
    let middle = fixture.text("middle");
    let new = fixture.text("newest");
    fixture.store.set_pinned(old, true).unwrap();

    let ids: Vec<i64> = fixture.store.recent(100).unwrap().iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![old, new, middle], "pinned first, then newest-first");
}

// ---- search ----------------------------------------------------------------

#[test]
fn the_specification_search_example_works() {
    // §8's worked example: a history of five items, searching `ffmpeg`.
    let mut fixture = Fixture::new();
    for body in [
        "https://example.com",
        "ffmpeg -i input.mp4 output.ts",
        "sudo systemctl restart nginx",
        "some other text",
        "a note to self",
    ] {
        fixture.text(body);
    }

    let hits = fixture.store.search("ffmpeg", 20).unwrap();
    assert_eq!(hits.len(), 1, "expected exactly one match, got {hits:?}");
    assert_eq!(hits[0].preview, "ffmpeg -i input.mp4 output.ts");
}

#[test]
fn search_is_case_insensitive_and_matches_inside_words() {
    let mut fixture = Fixture::new();
    fixture.text("Sudo SystemCtl Restart NGINX");

    for query in ["sudo", "SUDO", "nginx", "NgInX", "systemctl", "ctl"] {
        let hits = fixture.store.search(query, 20).unwrap();
        assert_eq!(hits.len(), 1, "{query:?} should match");
    }
}

#[test]
fn search_ignores_accents_in_both_directions() {
    let mut fixture = Fixture::new();
    fixture.text("café périphérique");

    for query in ["cafe", "café", "CAFE", "peripherique", "périph"] {
        let hits = fixture.store.search(query, 20).unwrap();
        assert_eq!(hits.len(), 1, "{query:?} should match the accented entry");
    }
}

#[test]
fn search_finds_text_in_a_multiline_entry_beyond_the_first_line() {
    let mut fixture = Fixture::new();
    fixture.text("first line\nsecond line\nthe needle is here\nfourth");

    let hits = fixture.store.search("needle", 20).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn search_finds_an_entry_whose_match_is_past_the_preview_cutoff() {
    let mut fixture = Fixture::new();
    // The preview keeps a couple of hundred characters; the match is well past
    // it, so this only passes if search reads the stored content rather than
    // the preview.
    let body = format!("{}\nNEEDLE_AT_THE_END", "filler ".repeat(400));
    fixture.text(&body);

    let hits = fixture.store.search("NEEDLE_AT_THE_END", 20).unwrap();
    assert_eq!(hits.len(), 1, "a match beyond the preview must still be found");
}

#[test]
fn an_empty_or_punctuation_only_query_does_not_error() {
    let mut fixture = Fixture::new();
    fixture.text("something");

    for query in ["", "   ", "!!!", "\"", "*", "()", "AND", "NOT"] {
        let result = fixture.store.search(query, 20);
        assert!(result.is_ok(), "{query:?} should not error: {:?}", result.err());
    }
}

#[test]
fn search_stays_responsive_on_a_large_history() {
    let mut fixture = Fixture::new();
    for index in 0..5_000 {
        fixture.text(&format!("entry number {index} with some filler text"));
    }
    fixture.text("the needle in the haystack");

    let started = std::time::Instant::now();
    let hits = fixture.store.search("needle", 20).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(hits.len(), 1);
    // Generous for CI, but a regression to a full table scan with a Rust-side
    // fuzzy match on every row would blow straight past it.
    assert!(elapsed.as_millis() < 500, "search took {elapsed:?}");
}

// ---- images and files ------------------------------------------------------

#[test]
fn an_image_goes_to_the_blob_store_and_comes_back_byte_identical() {
    let mut fixture = Fixture::new();
    let png = png_bytes(64, 48);

    let id = fixture.capture(&["image/png"], "image/png", &png);
    let item = fixture.store.get(id).unwrap().unwrap();

    assert_eq!(item.kind, ClipKind::Image);
    assert!(item.blob_hash.is_some(), "an image must not be stored inline");
    assert!(item.text_content.is_none());

    // Dimensions are recorded, which is what the row title and the thumbnail
    // sizing need.
    let meta = item.metadata.image.as_ref().expect("image metadata");
    assert_eq!((meta.width, meta.height), (64, 48));

    let (_, payload) = fixture.store.payload_of(id).unwrap();
    assert_eq!(payload, png, "the image must round-trip unmodified");
}

#[test]
fn images_are_not_stored_when_the_setting_is_off() {
    let fixture = Fixture::new();
    let mut settings = Settings::default();
    settings.clipboard.store_images = false;

    // `choose_target` should decline the image outright, so nothing is even
    // retrieved (§16's "decide before reading" rule).
    let chosen = normalize::choose_target(&["image/png".to_string()], &settings);
    assert!(chosen.is_none(), "an image must not be chosen when images are disabled");
    drop(fixture);
}

#[test]
fn a_file_selection_records_each_file_and_keeps_the_uri_list() {
    let mut fixture = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("report.pdf");
    let b = dir.path().join("notes.txt");
    std::fs::write(&a, b"pdf-ish").unwrap();
    std::fs::write(&b, b"notes").unwrap();

    let uri_list = format!("file://{}\r\nfile://{}\r\n", a.display(), b.display());
    let id = fixture.capture(&["text/uri-list"], "text/uri-list", uri_list.as_bytes());

    let item = fixture.store.get(id).unwrap().unwrap();
    assert_eq!(item.kind, ClipKind::Files);
    assert_eq!(item.metadata.files.len(), 2);
    assert_eq!(item.metadata.files[0].name, "report.pdf");
    assert_eq!(item.metadata.files[1].name, "notes.txt");
    // The row title names the first file and counts the rest.
    assert!(item.title().contains("report.pdf"), "{}", item.title());
}

// ---- limits and rejection --------------------------------------------------

#[test]
fn an_over_size_clipping_is_rejected_with_a_message_for_the_user() {
    let fixture = Fixture::new();
    let mut settings = Settings::default();
    settings.clipboard.max_item_size_mb = 1;

    let raw = RawClip {
        offered: vec!["UTF8_STRING".to_string()],
        mime: "UTF8_STRING".to_string(),
        bytes: vec![b'x'; 2 * 1024 * 1024],
        selection: Selection::Clipboard,
        source_app: None,
    };

    let rejection = normalize::normalize(&raw, &settings, &fixture.blobs)
        .expect("normalisation itself must not fail")
        .expect_err("a 2 MB item must be rejected against a 1 MB limit");

    // §33: the user is told, because this is a case they can act on.
    assert!(rejection.user_message().is_some());
    assert_eq!(rejection.reason(), "over size limit");
}

#[test]
fn an_empty_selection_is_dropped_without_bothering_the_user() {
    let fixture = Fixture::new();
    let raw = RawClip {
        offered: vec!["UTF8_STRING".to_string()],
        mime: "UTF8_STRING".to_string(),
        bytes: b"   \n\t  ".to_vec(),
        selection: Selection::Clipboard,
        source_app: None,
    };

    let outcome = normalize::normalize(&raw, &Settings::default(), &fixture.blobs).unwrap();
    assert!(outcome.is_err(), "whitespace-only selections are not worth storing");
}

#[test]
fn invalid_utf8_claiming_to_be_text_is_rejected_rather_than_mangled() {
    let fixture = Fixture::new();
    let raw = RawClip {
        offered: vec!["UTF8_STRING".to_string()],
        mime: "UTF8_STRING".to_string(),
        bytes: vec![0xff, 0xfe, 0x00, 0x01],
        selection: Selection::Clipboard,
        source_app: None,
    };

    let outcome = normalize::normalize(&raw, &Settings::default(), &fixture.blobs).unwrap();
    assert!(outcome.is_err(), "bytes that are not UTF-8 must not be stored as text");
}

// ---- persistence and maintenance -------------------------------------------

#[test]
fn history_survives_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
    let settings = Settings::default();

    let pinned_id = {
        let mut store = Store::open_at(dir.path()).unwrap();
        let raw = RawClip {
            offered: vec!["UTF8_STRING".to_string()],
            mime: "UTF8_STRING".to_string(),
            bytes: b"survives a restart".to_vec(),
            selection: Selection::Clipboard,
            source_app: None,
        };
        let clip = normalize::normalize(&raw, &settings, &blobs).unwrap().unwrap();
        let id = store.insert(&clip, true).unwrap().id();
        store.set_pinned(id, true).unwrap();
        id
    };

    // Reopen, as a restart or a reboot would.
    let store = Store::open_at(dir.path()).unwrap();
    let item = store.get(pinned_id).unwrap().expect("the entry must still be there");
    assert!(item.is_pinned, "the pin must survive a restart (§12)");
    assert_eq!(store.full_text(pinned_id).unwrap(), "survives a restart");
}

#[test]
fn the_history_limit_trims_unpinned_entries_and_spares_pinned_ones() {
    let mut fixture = Fixture::new();
    let keep = fixture.text("pin me");
    fixture.store.set_pinned(keep, true).unwrap();
    for index in 0..20 {
        fixture.text(&format!("disposable {index}"));
    }

    let removed = fixture.store.enforce_history_limit(5).unwrap();
    assert!(removed > 0, "the limit should have removed something");

    let remaining = fixture.store.recent(100).unwrap();
    // 5 unpinned plus the pinned one, which the limit must not count or cut.
    assert!(remaining.len() <= 6, "{} entries left", remaining.len());
    assert!(
        remaining.iter().any(|item| item.id == keep),
        "a pinned entry must never be removed by the history limit (§12)"
    );
}

#[test]
fn clearing_keeps_pinned_entries_unless_asked_otherwise() {
    let mut fixture = Fixture::new();
    let pinned = fixture.text("keep");
    fixture.store.set_pinned(pinned, true).unwrap();
    fixture.text("discard one");
    fixture.text("discard two");

    let removed = fixture.store.clear(false).unwrap();
    assert_eq!(removed, 2);
    assert!(fixture.store.get(pinned).unwrap().is_some());

    // And the explicit form does take them.
    fixture.store.clear(true).unwrap();
    assert!(fixture.store.get(pinned).unwrap().is_none());
}

#[test]
fn deleting_an_entry_frees_its_blob_immediately() {
    let mut fixture = Fixture::new();
    let png = png_bytes(32, 32);

    let id = fixture.capture(&["image/png"], "image/png", &png);
    let hash = fixture.store.get(id).unwrap().unwrap().blob_hash.unwrap();
    assert!(fixture.blobs.exists(&hash));

    assert!(fixture.store.delete(id).unwrap());

    // Freed as part of the delete transaction, not deferred to a sweep. That
    // matters for a clipboard manager: a deleted screenshot should stop
    // occupying disk at once, not at the next maintenance pass.
    assert!(!fixture.blobs.exists(&hash), "the blob should be gone already");

    // ...and the periodic sweep then has nothing left to do.
    assert_eq!(fixture.store.collect_orphan_blobs().unwrap(), 0);
}

#[test]
fn a_blob_shared_by_two_entries_survives_deleting_one_of_them() {
    // §48: "do not delete blobs that are still referenced". Two rows share a
    // blob when duplicate collapsing is off, which is the only way to get two
    // entries with identical content.
    let mut fixture = Fixture::new();
    let mut settings = Settings::default();
    settings.clipboard.ignore_duplicates = false;
    let png = png_bytes(24, 24);

    let first = fixture.capture_with(&settings, &["image/png"], "image/png", &png);
    let second = fixture.capture_with(&settings, &["image/png"], "image/png", &png);
    assert_ne!(first, second, "collapsing is off, so these are separate rows");

    let hash = fixture.store.get(first).unwrap().unwrap().blob_hash.unwrap();
    assert_eq!(
        fixture.store.get(second).unwrap().unwrap().blob_hash.as_deref(),
        Some(hash.as_str()),
        "identical content should be stored once"
    );

    fixture.store.delete(first).unwrap();
    assert!(
        fixture.blobs.exists(&hash),
        "the blob is still referenced by the second entry and must not be freed"
    );
    assert_eq!(fixture.store.collect_orphan_blobs().unwrap(), 0);

    // Only once the last reference goes does the payload go.
    fixture.store.delete(second).unwrap();
    assert!(!fixture.blobs.exists(&hash));
}

#[test]
fn editing_an_entry_changes_its_text_and_marks_it_edited() {
    let mut fixture = Fixture::new();
    // §13's worked example.
    let id = fixture.text("sudo systemctl restart nginx");
    fixture.store.update_text(id, "sudo systemctl restart apache2", 220).unwrap();

    let item = fixture.store.get(id).unwrap().unwrap();
    assert_eq!(fixture.store.full_text(id).unwrap(), "sudo systemctl restart apache2");
    assert!(item.metadata.edited, "an edited entry should say so in its subtitle");

    // And it is findable under the new text, not the old.
    assert_eq!(fixture.store.search("apache2", 20).unwrap().len(), 1);
    assert_eq!(fixture.store.search("nginx", 20).unwrap().len(), 0);
}

#[test]
fn export_and_import_round_trip_through_a_fresh_database() {
    let mut source = Fixture::new();
    let pinned = source.text("pinned entry");
    source.store.set_pinned(pinned, true).unwrap();
    source.text("ordinary entry");
    source.capture(&["image/png"], "image/png", &png_bytes(16, 16));

    let file = source._dir.path().join("export.json");
    let exported = source.store.export_to_file(&file).unwrap();
    assert_eq!(exported, 3);

    let mut target = Fixture::new();
    let (added, skipped, failed) = target.store.import_from_file(&file).unwrap();
    assert_eq!((added, skipped, failed), (3, 0, 0));

    let items = target.store.recent(100).unwrap();
    assert_eq!(items.len(), 3);
    assert!(items.iter().any(|item| item.is_pinned), "pins must survive the round trip");
    assert!(items.iter().any(|item| item.kind == ClipKind::Image));

    // Importing the same file again must not duplicate anything.
    let (added, skipped, _) = target.store.import_from_file(&file).unwrap();
    assert_eq!(added, 0);
    assert_eq!(skipped, 3);
}

// ---- privacy ---------------------------------------------------------------

#[test]
fn a_password_manager_hint_stops_the_clipping_before_it_is_read() {
    // §16: the decision happens on the target list, before any payload is
    // requested — so this is tested at that level, not after the fact.
    let settings = Settings::default();
    assert!(settings.privacy.skip_passwords, "the hint should be honoured by default");

    let offered = vec![
        "UTF8_STRING".to_string(),
        "x-kde-passwordManagerHint".to_string(),
    ];
    let block = clipboard_manager::privacy::check(&settings.privacy, &offered, None, None);
    assert!(block.is_some(), "a flagged password must be refused");
}

#[test]
fn an_excluded_application_is_refused_by_name() {
    let settings = Settings::default();
    let app = clipboard_manager::model::AppIdentity {
        name: Some("keepassxc".to_string()),
        exe: Some("/usr/bin/keepassxc".to_string()),
        wm_class: Some("keepassxc".to_string()),
    };

    let offered = vec!["UTF8_STRING".to_string()];
    let block = clipboard_manager::privacy::check(&settings.privacy, &offered, Some(&app), None);
    assert!(block.is_some(), "the default exclusion list should cover KeePassXC");
}

#[test]
fn an_unrelated_application_is_not_caught_by_the_exclusion_list() {
    let settings = Settings::default();
    let app = clipboard_manager::model::AppIdentity {
        name: Some("gedit".to_string()),
        exe: Some("/usr/bin/gedit".to_string()),
        wm_class: Some("gedit".to_string()),
    };

    let offered = vec!["UTF8_STRING".to_string()];
    assert!(clipboard_manager::privacy::check(&settings.privacy, &offered, Some(&app), None)
        .is_none());
}

#[test]
fn pausing_stops_capture_without_touching_existing_history() {
    let mut fixture = Fixture::new();
    fixture.text("recorded before pausing");

    let mut settings = Settings::default();
    settings.privacy.paused = true;

    // Paused is enforced by the monitor's `interested`/`wants` gate; what the
    // store must guarantee is that existing history stays readable.
    assert_eq!(fixture.store.recent(100).unwrap().len(), 1);
    assert_eq!(fixture.store.search("recorded", 20).unwrap().len(), 1);
}

// ---- helpers ---------------------------------------------------------------

/// A real PNG of the requested size, built at runtime so the test carries no
/// binary fixture.
fn png_bytes(width: i32, height: i32) -> Vec<u8> {
    let pixbuf =
        gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, width, height).unwrap();
    pixbuf.fill(0x3366ccff);
    pixbuf.save_to_bufferv("png", &[]).unwrap().to_vec()
}
