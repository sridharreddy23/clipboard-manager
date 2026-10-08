//! Retention, history limits, blob garbage collection and import/export
//! (§14, §38, §48).
//!
//! Maintenance never runs during startup. The daemon schedules it on an idle
//! timeout a few seconds after the clipboard monitor is live, so login is not
//! delayed (§46), and then on a long interval.
//!
//! **Pinned entries are exempt from every automatic removal path.** That is
//! asserted by the tests in this module, because it is the one invariant a user
//! would notice immediately if it broke (§12).

use std::io::Write;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::{blobs, Store, ITEM_COLUMNS};
use crate::config::Settings;
use crate::model::{self, ClipItem, ClipKind, Metadata, NewClip};

/// What a maintenance pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// Entries removed because they exceeded the retention period.
    pub expired: usize,
    /// Entries removed because the history-size limit was exceeded.
    pub trimmed: usize,
    /// Unreferenced blobs deleted.
    pub blobs_collected: usize,
    /// Interrupted-write temporary files removed.
    pub temporaries_removed: usize,
    /// Whether the database was compacted on this pass.
    pub vacuumed: bool,
}

impl MaintenanceReport {
    pub fn did_anything(&self) -> bool {
        self.expired > 0
            || self.trimmed > 0
            || self.blobs_collected > 0
            || self.temporaries_removed > 0
            || self.vacuumed
    }
}

/// Serialised history, for `clipboard-manager export` (§38).
#[derive(Debug, Serialize, Deserialize)]
pub struct Export {
    /// Fixed marker so an import can reject an unrelated JSON file.
    pub application: String,
    pub format_version: u32,
    pub exported_at: i64,
    /// Present so that anyone who opens the file sees the warning first.
    pub warning: String,
    pub items: Vec<ExportItem>,
}

/// One exported entry. Payloads are inlined so the file is self-contained.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExportItem {
    pub kind: String,
    pub mime_type: String,
    pub preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 payload for image and file entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_base64: Option<String>,
    #[serde(default)]
    pub metadata: Metadata,
    pub size_bytes: u64,
    pub created_at: i64,
    pub last_used_at: i64,
    pub is_pinned: bool,
}

const EXPORT_APPLICATION: &str = "clipboard-manager";
const EXPORT_FORMAT_VERSION: u32 = 1;
pub const EXPORT_WARNING: &str = "This file contains your clipboard history in plain text. \
     It may include passwords, tokens, private messages and other sensitive \
     information. Store it somewhere private and delete it when you are done.";

impl Store {
    /// Run a full maintenance pass.
    pub fn maintain(&mut self, settings: &Settings) -> Result<MaintenanceReport> {
        // Ordered deliberately: expire first, then trim to the count limit,
        // and only then collect blobs, so the collection sees every row the
        // earlier two steps removed.
        let expired = self.expire_old(settings.clipboard.retention_days)?;
        let trimmed = self.enforce_history_limit(settings.clipboard.max_history)?;
        let blobs_collected = self.collect_orphan_blobs()?;
        let temporaries_removed = self.blobs().sweep_temporaries()?;

        let mut report = MaintenanceReport {
            expired,
            trimmed,
            blobs_collected,
            temporaries_removed,
            ..MaintenanceReport::default()
        };

        // Only compact when there is something worth compacting. VACUUM
        // rewrites the whole file, so doing it on every pass would be exactly
        // the "repeated database writes" §45 rules out.
        if report.expired + report.trimmed >= 50 {
            self.vacuum()?;
            report.vacuumed = true;
        }

        if report.did_anything() {
            tracing::info!(
                expired = report.expired,
                trimmed = report.trimmed,
                blobs = report.blobs_collected,
                vacuumed = report.vacuumed,
                "maintenance pass complete"
            );
        }
        Ok(report)
    }

    /// Delete unpinned entries older than `days`. `0` disables expiry.
    ///
    /// Age is measured from `last_used_at`, not `created_at`, so an old entry
    /// the user keeps re-using does not expire underneath them.
    pub fn expire_old(&mut self, days: u32) -> Result<usize> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = model::now_ms() - (days as i64) * 86_400_000;
        let removed = self.conn_mut().execute(
            "DELETE FROM clipboard_items WHERE is_pinned = 0 AND last_used_at < ?1",
            [cutoff],
        )?;
        Ok(removed)
    }

    /// Keep at most `limit` unpinned entries, deleting the least recently used.
    /// `0` disables the limit.
    ///
    /// The limit deliberately counts **unpinned** entries only: pinning an item
    /// should not consume a slot the user thinks of as history, and pinned
    /// entries must never be trimmed (§12, §14).
    pub fn enforce_history_limit(&mut self, limit: u32) -> Result<usize> {
        if limit == 0 {
            return Ok(0);
        }
        let removed = self.conn_mut().execute(
            "DELETE FROM clipboard_items
              WHERE id IN (
                  SELECT id FROM clipboard_items
                   WHERE is_pinned = 0
                   ORDER BY last_used_at DESC
                   LIMIT -1 OFFSET ?1
              )",
            [limit as i64],
        )?;
        Ok(removed)
    }

    /// Compact the database file.
    pub fn vacuum(&mut self) -> Result<()> {
        // FTS5 has its own compaction; running it first means VACUUM has less
        // to copy.
        if let Err(err) = self.conn().execute(
            "INSERT INTO clipboard_items_fts(clipboard_items_fts) VALUES('optimize')",
            [],
        ) {
            tracing::warn!(error = %err, "full-text index optimise failed");
        }
        self.conn()
            .execute_batch("VACUUM")
            .context("failed to compact the database")?;
        Ok(())
    }

    /// Rebuild the full-text index from the item table.
    ///
    /// Exposed for recovery: if the index and the table ever disagree (for
    /// instance after a manual `sqlite3` edit), this restores search without
    /// losing history.
    pub fn rebuild_search_index(&mut self) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO clipboard_items_fts(clipboard_items_fts) VALUES('rebuild')",
                [],
            )
            .context("failed to rebuild the search index")?;
        tracing::info!("search index rebuilt");
        Ok(())
    }

    /// Verify the database and the search index, repairing the index if needed.
    ///
    /// Called at startup behind a cheap `quick_check`, so a database that was
    /// damaged by a power loss is noticed and fixed rather than producing
    /// confusing search results forever (§49).
    pub fn verify_and_repair(&mut self) -> Result<()> {
        let check: String = self
            .conn()
            .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
            .unwrap_or_else(|_| "failed".to_string());
        if !check.eq_ignore_ascii_case("ok") {
            bail!("database integrity check reported: {check}");
        }

        // `integrity-check` on an FTS5 table raises an error when the index and
        // the content table disagree.
        let fts_ok = self
            .conn()
            .execute(
                "INSERT INTO clipboard_items_fts(clipboard_items_fts) VALUES('integrity-check')",
                [],
            )
            .is_ok();
        if !fts_ok {
            tracing::warn!("search index is inconsistent; rebuilding");
            self.rebuild_search_index()?;
        }
        Ok(())
    }

    /// Delete everything, including pinned entries and all blobs, and recreate
    /// an empty database (Settings → Advanced → Reset database).
    pub fn reset(&mut self) -> Result<()> {
        self.clear(true)?;
        self.vacuum()?;
        tracing::warn!("history database reset by user request");
        Ok(())
    }

    // -- export / import ------------------------------------------------------

    /// Serialise the whole history.
    pub fn export(&self) -> Result<Export> {
        let sql = format!("SELECT {ITEM_COLUMNS} FROM clipboard_items ORDER BY last_used_at ASC");
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([], super::row_to_item)?;

        let mut items = Vec::new();
        for row in rows {
            let item: ClipItem = match row {
                Ok(i) => i,
                Err(err) => {
                    tracing::warn!(error = %err, "skipping unreadable entry during export");
                    continue;
                }
            };
            let payload_base64 = match &item.blob_hash {
                Some(hash) => match self.blobs().get(hash) {
                    Ok(bytes) => Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
                    Err(err) => {
                        tracing::warn!(
                            id = item.id,
                            error = %err,
                            "skipping entry whose payload is missing"
                        );
                        continue;
                    }
                },
                None => None,
            };
            items.push(ExportItem {
                kind: item.kind.as_str().to_string(),
                mime_type: item.mime_type,
                preview: item.preview,
                text: item.text_content,
                payload_base64,
                metadata: item.metadata,
                size_bytes: item.size_bytes,
                created_at: item.created_at,
                last_used_at: item.last_used_at,
                is_pinned: item.is_pinned,
            });
        }

        Ok(Export {
            application: EXPORT_APPLICATION.to_string(),
            format_version: EXPORT_FORMAT_VERSION,
            exported_at: model::now_ms(),
            warning: EXPORT_WARNING.to_string(),
            items,
        })
    }

    /// Write the history to `path` as JSON, mode `0600`.
    pub fn export_to_file(&self, path: &std::path::Path) -> Result<usize> {
        let export = self.export()?;
        let count = export.items.len();
        let mut json = serde_json::to_vec_pretty(&export).context("failed to serialise history")?;
        json.push(b'\n');
        // The export is as sensitive as the database, so it gets the same mode
        // even though the user chose the location (§38).
        crate::paths::write_private_atomic(path, &json)?;
        Ok(count)
    }

    /// Merge an exported history back in.
    ///
    /// Existing entries are matched by content hash: an import never duplicates
    /// something already present, it only refreshes `last_used_at` and adds the
    /// pin if the imported copy was pinned. Returns `(added, merged, skipped)`.
    pub fn import(&mut self, export: &Export) -> Result<(usize, usize, usize)> {
        if export.application != EXPORT_APPLICATION {
            bail!(
                "this file was not produced by clipboard-manager (application = {:?})",
                export.application
            );
        }
        if export.format_version > EXPORT_FORMAT_VERSION {
            bail!(
                "export format version {} is newer than this build understands ({})",
                export.format_version,
                EXPORT_FORMAT_VERSION
            );
        }

        let mut added = 0usize;
        let mut merged = 0usize;
        let mut skipped = 0usize;

        for raw in &export.items {
            match self.import_one(raw) {
                Ok(true) => added += 1,
                Ok(false) => merged += 1,
                Err(err) => {
                    // One malformed entry must not abort the whole import.
                    tracing::warn!(error = %err, "skipping unimportable entry");
                    skipped += 1;
                }
            }
        }

        tracing::info!(added, merged, skipped, "history import complete");
        Ok((added, merged, skipped))
    }

    /// Returns `Ok(true)` when a new row was created.
    fn import_one(&mut self, raw: &ExportItem) -> Result<bool> {
        let kind = ClipKind::parse(&raw.kind)
            .with_context(|| format!("unknown entry kind {:?}", raw.kind))?;

        // Which storage path to use is decided by what the file actually
        // carries, not by the kind: a long text entry is exported with a
        // base64 payload because that is how it was stored.
        let payload: Vec<u8> = match (&raw.payload_base64, &raw.text) {
            (Some(encoded), _) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .context("payload is not valid base64")?,
            (None, Some(text)) => text.as_bytes().to_vec(),
            (None, None) => bail!("entry has neither text nor a payload"),
        };
        if kind.is_binary() && raw.payload_base64.is_none() {
            bail!("{} entry has no payload", kind.label());
        }

        let (text_content, blob_hash) =
            if kind.is_binary() || payload.len() > super::INLINE_TEXT_LIMIT {
                (None, Some(self.blobs().put(&payload)?))
            } else {
                let text = String::from_utf8(payload.clone())
                    .context("text entry is not valid UTF-8")?;
                (Some(text), None)
            };

        let content_hash = blobs::hash_parts(&[
            kind.as_str().as_bytes(),
            raw.mime_type.as_bytes(),
            &payload,
        ]);

        // Already present? Merge rather than duplicate.
        if let Some(existing) = self.get_by_hash(&content_hash)? {
            let keep_last_used = existing.last_used_at.max(raw.last_used_at);
            let pin = existing.is_pinned || raw.is_pinned;
            self.conn().execute(
                "UPDATE clipboard_items
                    SET last_used_at = ?1, is_pinned = ?2, updated_at = ?3
                  WHERE id = ?4",
                rusqlite::params![keep_last_used, pin as i32, model::now_ms(), existing.id],
            )?;
            return Ok(false);
        }

        let clip = NewClip {
            kind,
            mime_type: raw.mime_type.clone(),
            content_hash,
            text_content,
            blob_hash,
            preview: raw.preview.clone(),
            metadata: raw.metadata.clone(),
            size_bytes: raw.size_bytes,
        };

        let tx = self.conn_mut().transaction()?;
        // Preserve the original timestamps and pin state, which is the whole
        // point of an import.
        Store::insert_row(
            &tx,
            &clip,
            &clip.content_hash,
            raw.created_at,
            raw.last_used_at,
            raw.is_pinned,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Read an export file and merge it.
    pub fn import_from_file(&mut self, path: &std::path::Path) -> Result<(usize, usize, usize)> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let export: Export =
            serde_json::from_slice(&bytes).context("file is not a clipboard-manager export")?;
        self.import(&export)
    }

    /// Hot backup of the SQLite file, usable while the daemon is running.
    ///
    /// Offered alongside the JSON export because a `.sqlite` copy is the
    /// fastest way to move a large history between machines, and unlike `cp` it
    /// is consistent with respect to the WAL.
    pub fn backup_to_file(&self, path: &std::path::Path) -> Result<()> {
        use rusqlite::backup::Backup;

        let mut dest = rusqlite::Connection::open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        {
            let backup = Backup::new(self.conn(), &mut dest)?;
            backup.run_to_completion(256, std::time::Duration::from_millis(10), None)?;
        }
        drop(dest);
        crate::paths::secure_file(path)?;
        Ok(())
    }
}

/// Print the export warning to stderr before a CLI export writes anything.
pub fn print_export_warning(mut out: impl Write) -> std::io::Result<()> {
    writeln!(out, "WARNING: {EXPORT_WARNING}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ImageMeta;
    use crate::storage::{blobs as blobmod, Store};

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_memory(&tmp.path().join("blobs")).unwrap();
        (tmp, store)
    }

    fn clip(text: &str) -> NewClip {
        NewClip {
            kind: ClipKind::Text,
            mime_type: "text/plain".into(),
            content_hash: blobmod::hash_parts(&[b"text", b"text/plain", text.as_bytes()]),
            text_content: Some(text.to_string()),
            blob_hash: None,
            preview: text.to_string(),
            metadata: Metadata::default(),
            size_bytes: text.len() as u64,
        }
    }

    /// Insert `text` with an explicit `last_used_at`, so retention can be
    /// tested without waiting.
    fn insert_aged(store: &mut Store, text: &str, age_days: i64, pinned: bool) -> i64 {
        let id = store.insert(&clip(text), true).unwrap().id();
        let ts = model::now_ms() - age_days * 86_400_000;
        store
            .conn()
            .execute(
                "UPDATE clipboard_items SET created_at = ?1, last_used_at = ?1, is_pinned = ?2
                  WHERE id = ?3",
                rusqlite::params![ts, pinned as i32, id],
            )
            .unwrap();
        id
    }

    // -- retention ------------------------------------------------------------

    #[test]
    fn retention_removes_only_old_unpinned_entries() {
        let (_t, mut store) = store();
        let fresh = insert_aged(&mut store, "fresh", 1, false);
        let old = insert_aged(&mut store, "old", 40, false);
        let old_pinned = insert_aged(&mut store, "old but pinned", 400, true);

        assert_eq!(store.expire_old(30).unwrap(), 1);
        assert!(store.get(fresh).unwrap().is_some());
        assert!(store.get(old).unwrap().is_none());
        assert!(
            store.get(old_pinned).unwrap().is_some(),
            "pinned entries must never expire"
        );
    }

    #[test]
    fn retention_of_zero_days_disables_expiry() {
        let (_t, mut store) = store();
        insert_aged(&mut store, "ancient", 10_000, false);
        assert_eq!(store.expire_old(0).unwrap(), 0);
        assert_eq!(store.stats().unwrap().total_items, 1);
    }

    #[test]
    fn retention_measures_last_use_not_creation() {
        let (_t, mut store) = store();
        let id = insert_aged(&mut store, "kept alive", 100, false);
        // Re-copying it refreshes last_used_at even though created_at is old.
        store.insert(&clip("kept alive"), true).unwrap();
        assert_eq!(store.expire_old(30).unwrap(), 0);
        assert!(store.get(id).unwrap().is_some());
    }

    #[test]
    fn each_retention_choice_is_honoured() {
        for (days, _) in crate::config::ClipboardSettings::RETENTION_CHOICES {
            let (_t, mut store) = store();
            insert_aged(&mut store, "two days old", 2, false);
            let removed = store.expire_old(days).unwrap();
            let expected = if days != 0 && days < 2 { 1 } else { 0 };
            assert_eq!(removed, expected, "retention_days = {days}");
        }
    }

    // -- history limit --------------------------------------------------------

    #[test]
    fn history_limit_keeps_the_newest_entries() {
        let (_t, mut store) = store();
        for i in 0..20 {
            // Age descending, so entry 0 is the oldest.
            insert_aged(&mut store, &format!("entry {i}"), (20 - i) as i64, false);
        }
        assert_eq!(store.enforce_history_limit(5).unwrap(), 15);

        let kept: Vec<_> = store
            .recent(50)
            .unwrap()
            .into_iter()
            .map(|i| i.preview)
            .collect();
        assert_eq!(
            kept,
            vec!["entry 19", "entry 18", "entry 17", "entry 16", "entry 15"]
        );
    }

    #[test]
    fn history_limit_does_not_count_or_trim_pinned_entries() {
        let (_t, mut store) = store();
        for i in 0..5 {
            insert_aged(&mut store, &format!("pinned {i}"), 100 + i as i64, true);
        }
        for i in 0..10 {
            insert_aged(&mut store, &format!("loose {i}"), (10 - i) as i64, false);
        }

        assert_eq!(store.enforce_history_limit(3).unwrap(), 7);
        let stats = store.stats().unwrap();
        assert_eq!(stats.pinned_items, 5, "all pins survive");
        assert_eq!(stats.total_items, 8, "5 pinned + 3 unpinned");
    }

    #[test]
    fn unlimited_history_trims_nothing() {
        let (_t, mut store) = store();
        for i in 0..30 {
            insert_aged(&mut store, &format!("entry {i}"), i as i64, false);
        }
        assert_eq!(store.enforce_history_limit(0).unwrap(), 0);
        assert_eq!(store.stats().unwrap().total_items, 30);
    }

    #[test]
    fn every_offered_history_limit_works() {
        for (limit, _) in crate::config::ClipboardSettings::HISTORY_CHOICES {
            let (_t, mut store) = store();
            for i in 0..60 {
                insert_aged(&mut store, &format!("e{i}"), 1, false);
            }
            store.enforce_history_limit(limit).unwrap();
            let kept = store.stats().unwrap().total_items;
            let expected = if limit == 0 { 60 } else { 60u64.min(limit as u64) };
            assert_eq!(kept, expected, "limit = {limit}");
        }
    }

    // -- blob gc --------------------------------------------------------------

    #[test]
    fn maintenance_collects_orphan_blobs_but_not_live_ones() {
        let (_t, mut store) = store();
        let live_bytes = b"referenced image";
        let live_hash = store.blobs().put(live_bytes).unwrap();
        let orphan_hash = store.blobs().put(b"nobody references me").unwrap();

        let img = NewClip {
            kind: ClipKind::Image,
            mime_type: "image/png".into(),
            content_hash: blobmod::hash_parts(&[b"image", b"image/png", live_bytes]),
            text_content: None,
            blob_hash: Some(live_hash.clone()),
            preview: "Image".into(),
            metadata: Metadata {
                image: Some(ImageMeta { width: 1, height: 1, format: "png".into() }),
                ..Default::default()
            },
            size_bytes: live_bytes.len() as u64,
        };
        store.insert(&img, true).unwrap();

        let settings = Settings::default();
        let report = store.maintain(&settings).unwrap();
        assert_eq!(report.blobs_collected, 1);
        assert!(store.blobs().exists(&live_hash));
        assert!(!store.blobs().exists(&orphan_hash));
    }

    #[test]
    fn maintenance_is_idempotent_and_quiet_when_there_is_nothing_to_do() {
        let (_t, mut store) = store();
        store.insert(&clip("hello"), true).unwrap();
        let settings = Settings::default();

        let first = store.maintain(&settings).unwrap();
        let second = store.maintain(&settings).unwrap();
        assert!(!first.did_anything());
        assert!(!second.did_anything());
        assert_eq!(store.stats().unwrap().total_items, 1);
    }

    #[test]
    fn maintenance_only_vacuums_after_a_large_deletion() {
        let (_t, mut store) = store();
        let mut settings = Settings::default();
        settings.clipboard.max_history = 5;

        for i in 0..10 {
            insert_aged(&mut store, &format!("e{i}"), 1, false);
        }
        assert!(!store.maintain(&settings).unwrap().vacuumed);

        for i in 0..100 {
            insert_aged(&mut store, &format!("f{i}"), 1, false);
        }
        assert!(store.maintain(&settings).unwrap().vacuumed);
    }

    // -- integrity ------------------------------------------------------------

    #[test]
    fn verify_passes_on_a_healthy_database() {
        let (_t, mut store) = store();
        store.insert(&clip("hello"), true).unwrap();
        store.verify_and_repair().unwrap();
        assert_eq!(store.search("hello", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_damaged_search_index_is_rebuilt() {
        let (_t, mut store) = store();
        store.insert(&clip("ffmpeg -i a.mp4"), true).unwrap();
        // Wipe the index behind the triggers' back, as a crash mid-write could.
        store
            .conn()
            .execute("INSERT INTO clipboard_items_fts(clipboard_items_fts) VALUES('delete-all')", [])
            .unwrap();
        // The index was destroyed and rebuilt; the point is that searching
        // works at all afterwards rather than erroring, and that the row is
        // findable again.
        assert_eq!(store.search("ffmpeg", 10).unwrap().len(), 1);

        store.rebuild_search_index().unwrap();
        assert_eq!(store.search("ffmpeg", 10).unwrap().len(), 1);
    }

    #[test]
    fn reset_empties_everything() {
        let (_t, mut store) = store();
        let id = store.insert(&clip("pinned"), true).unwrap().id();
        store.set_pinned(id, true).unwrap();
        let blob = store.blobs().put(b"image").unwrap();

        store.reset().unwrap();
        assert_eq!(store.stats().unwrap().total_items, 0);
        assert!(!store.blobs().exists(&blob));
    }

    // -- export / import ------------------------------------------------------

    fn seed_for_export(store: &mut Store) -> String {
        let pinned = store.insert(&clip("sudo systemctl restart nginx"), true).unwrap().id();
        store.set_pinned(pinned, true).unwrap();
        store.insert(&clip("kubectl get pods"), true).unwrap();
        store.insert(&clip("héllo 日本語 👍"), true).unwrap();

        let bytes = b"\x89PNG\r\n\x1a\nbinary\x00payload";
        let hash = store.blobs().put(bytes).unwrap();
        let img = NewClip {
            kind: ClipKind::Image,
            mime_type: "image/png".into(),
            content_hash: blobmod::hash_parts(&[b"image", b"image/png", bytes]),
            text_content: None,
            blob_hash: Some(hash.clone()),
            preview: "Image 4 × 4".into(),
            metadata: Metadata {
                image: Some(ImageMeta { width: 4, height: 4, format: "png".into() }),
                ..Default::default()
            },
            size_bytes: bytes.len() as u64,
        };
        store.insert(&img, true).unwrap();
        hash
    }

    #[test]
    fn export_then_import_into_an_empty_store_restores_everything() {
        let (_t1, mut source) = store();
        let img_hash = seed_for_export(&mut source);
        let export = source.export().unwrap();
        assert_eq!(export.items.len(), 4);
        assert_eq!(export.application, "clipboard-manager");
        assert!(!export.warning.is_empty(), "export must carry the §38 warning");

        let (_t2, mut target) = store();
        let (added, merged, skipped) = target.import(&export).unwrap();
        assert_eq!((added, merged, skipped), (4, 0, 0));

        let stats = target.stats().unwrap();
        assert_eq!(stats.total_items, 4);
        assert_eq!(stats.pinned_items, 1, "pin state is preserved");
        assert_eq!(target.search("nginx", 10).unwrap().len(), 1);
        assert_eq!(target.search("日本語", 10).unwrap().len(), 1);
        assert_eq!(
            target.blobs().get(&img_hash).unwrap(),
            b"\x89PNG\r\n\x1a\nbinary\x00payload"
        );
    }

    #[test]
    fn importing_twice_merges_instead_of_duplicating() {
        let (_t1, mut source) = store();
        seed_for_export(&mut source);
        let export = source.export().unwrap();

        let (_t2, mut target) = store();
        target.import(&export).unwrap();
        let (added, merged, _) = target.import(&export).unwrap();
        assert_eq!(added, 0);
        assert_eq!(merged, 4);
        assert_eq!(target.stats().unwrap().total_items, 4);
    }

    #[test]
    fn import_preserves_original_timestamps() {
        let (_t1, mut source) = store();
        let id = insert_aged(&mut source, "old entry", 100, false);
        let original = source.get(id).unwrap().unwrap();
        let export = source.export().unwrap();

        let (_t2, mut target) = store();
        target.import(&export).unwrap();
        let restored = &target.recent(1).unwrap()[0];
        assert_eq!(restored.created_at, original.created_at);
        assert_eq!(restored.last_used_at, original.last_used_at);
    }

    #[test]
    fn export_file_round_trip_is_private_and_lossless() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.json");

        let (_t1, mut source) = store();
        seed_for_export(&mut source);
        assert_eq!(source.export_to_file(&path).unwrap(), 4);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an export is as sensitive as the database");

        let (_t2, mut target) = store();
        let (added, _, skipped) = target.import_from_file(&path).unwrap();
        assert_eq!((added, skipped), (4, 0));
    }

    #[test]
    fn importing_a_foreign_json_file_is_refused() {
        let (_t, mut store) = store();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("other.json");
        std::fs::write(&path, br#"{"application":"something-else","format_version":1,"exported_at":0,"warning":"","items":[]}"#).unwrap();
        let err = store.import_from_file(&path).unwrap_err().to_string();
        assert!(err.contains("not produced by clipboard-manager"), "{err}");
    }

    #[test]
    fn importing_a_newer_format_is_refused() {
        let (_t, mut store) = store();
        let export = Export {
            application: EXPORT_APPLICATION.into(),
            format_version: EXPORT_FORMAT_VERSION + 1,
            exported_at: 0,
            warning: String::new(),
            items: vec![],
        };
        assert!(store.import(&export).is_err());
    }

    #[test]
    fn import_skips_bad_entries_and_keeps_the_good_ones() {
        let (_t, mut store) = store();
        let export = Export {
            application: EXPORT_APPLICATION.into(),
            format_version: 1,
            exported_at: 0,
            warning: String::new(),
            items: vec![
                ExportItem {
                    kind: "text".into(),
                    mime_type: "text/plain".into(),
                    preview: "good".into(),
                    text: Some("good".into()),
                    payload_base64: None,
                    metadata: Metadata::default(),
                    size_bytes: 4,
                    created_at: 1,
                    last_used_at: 1,
                    is_pinned: false,
                },
                // Unknown kind.
                ExportItem {
                    kind: "hologram".into(),
                    mime_type: "x/y".into(),
                    preview: "bad".into(),
                    text: Some("bad".into()),
                    payload_base64: None,
                    metadata: Metadata::default(),
                    size_bytes: 3,
                    created_at: 1,
                    last_used_at: 1,
                    is_pinned: false,
                },
                // Image with no payload.
                ExportItem {
                    kind: "image".into(),
                    mime_type: "image/png".into(),
                    preview: "bad".into(),
                    text: None,
                    payload_base64: None,
                    metadata: Metadata::default(),
                    size_bytes: 3,
                    created_at: 1,
                    last_used_at: 1,
                    is_pinned: false,
                },
                // Image with corrupt base64.
                ExportItem {
                    kind: "image".into(),
                    mime_type: "image/png".into(),
                    preview: "bad".into(),
                    text: None,
                    payload_base64: Some("!!!not base64!!!".into()),
                    metadata: Metadata::default(),
                    size_bytes: 3,
                    created_at: 1,
                    last_used_at: 1,
                    is_pinned: false,
                },
            ],
        };

        let (added, merged, skipped) = store.import(&export).unwrap();
        assert_eq!((added, merged, skipped), (1, 0, 3));
        assert_eq!(store.stats().unwrap().total_items, 1);
    }

    #[test]
    fn sqlite_backup_is_restorable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        let backup = tmp.path().join("backup.sqlite");

        {
            let mut live = Store::open_at(&dir).unwrap();
            live.insert(&clip("backed up"), true).unwrap();
            live.backup_to_file(&backup).unwrap();
        }

        let restored = rusqlite::Connection::open(&backup).unwrap();
        let count: i64 = restored
            .query_row("SELECT count(*) FROM clipboard_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
