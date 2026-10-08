//! Persistent clipboard history (§15, §48, §49).
//!
//! [`Store`] owns the SQLite connection and the blob store. It is created and
//! used **only on the GTK main thread**; worker threads never touch it. That
//! single-owner rule is what lets the daemon avoid locking entirely, and it is
//! why the expensive part of handling a clipping (hashing and writing the blob)
//! happens on the monitor thread *before* the event is posted.

pub mod blobs;
pub mod maintenance;
pub mod schema;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OptionalExtension, Transaction};

pub use blobs::BlobStore;

use crate::config::Settings;
use crate::model::{self, ClipItem, ClipKind, Metadata, NewClip};
use crate::paths;
use crate::search;

/// Columns selected by a single-entry query, in the order [`row_to_item`]
/// expects.
const ITEM_COLUMNS: &str = "id, kind, mime_type, content_hash, text_content, blob_hash, \
                            preview, metadata, size_bytes, created_at, updated_at, \
                            last_used_at, is_pinned";

/// Columns selected by list and search queries.
///
/// `text_content` is deliberately replaced with `NULL`. A clipping can be
/// megabytes of text, and a search re-ranks up to 2 000 candidates on every
/// keystroke; selecting the real column would read every one of those payloads
/// out of SQLite's overflow pages and allocate them, which is exactly the
/// behaviour §45 and §47 rule out. Rows returned by these queries are
/// "preview only" (see [`ClipItem::is_preview_only`]) and the caller fetches
/// the payload with [`Store::payload_of`] when the user actually needs it.
const LIST_COLUMNS: &str = "id, kind, mime_type, content_hash, NULL, blob_hash, \
                            preview, metadata, size_bytes, created_at, updated_at, \
                            last_used_at, is_pinned";

/// Text longer than this is kept in the blob store instead of inline, so that
/// one enormous clipping cannot bloat the database file or the WAL (§33).
pub const INLINE_TEXT_LIMIT: usize = 256 * 1024;

/// Result of offering a clipping to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// A new history entry was created.
    Inserted { id: i64 },
    /// The content was already present; the existing entry was moved to the top
    /// and its pin state preserved (§10).
    Duplicate { id: i64 },
}

impl InsertOutcome {
    pub fn id(self) -> i64 {
        match self {
            InsertOutcome::Inserted { id } | InsertOutcome::Duplicate { id } => id,
        }
    }

    pub fn is_new(self) -> bool {
        matches!(self, InsertOutcome::Inserted { .. })
    }
}

/// Aggregate figures for `clipboard-manager status` and Settings → Advanced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub total_items: u64,
    pub pinned_items: u64,
    pub database_bytes: u64,
    pub blob_bytes: u64,
    pub blob_count: u64,
}

impl Stats {
    pub fn total_bytes(&self) -> u64 {
        self.database_bytes + self.blob_bytes
    }
}

pub struct Store {
    conn: Connection,
    blobs: BlobStore,
    data_dir: PathBuf,
    db_path: PathBuf,
}

impl Store {
    /// Open the store described by `settings`, creating it if necessary.
    pub fn open(settings: &Settings) -> Result<Self> {
        Self::open_at(&settings.data_dir())
    }

    /// Open (or create) a store rooted at `data_dir`.
    ///
    /// If the database cannot be opened or migrated because it is corrupt, it
    /// is moved aside and a fresh one is created rather than leaving the daemon
    /// unable to start (§35, §49). Blobs are kept, because the pinned entries
    /// that referenced them may be recoverable from a backup, and maintenance
    /// will collect them otherwise.
    pub fn open_at(data_dir: &Path) -> Result<Self> {
        paths::ensure_private_dir(data_dir)?;
        let db_path = data_dir.join("database.sqlite");
        let blobs = BlobStore::open(data_dir.join("blobs"))?;

        let conn = match Self::connect(&db_path) {
            Ok(conn) => conn,
            Err(err) => {
                let stamp = model::now_ms();
                let quarantine = data_dir.join(format!("database.sqlite.corrupt-{stamp}"));
                tracing::error!(
                    error = %err,
                    quarantine = %quarantine.display(),
                    "database is unusable; quarantining it and starting fresh"
                );
                // Move the side files too, or SQLite will try to replay a WAL
                // that belongs to the quarantined database.
                for suffix in ["", "-wal", "-shm"] {
                    let from = PathBuf::from(format!("{}{suffix}", db_path.display()));
                    let to = PathBuf::from(format!("{}{suffix}", quarantine.display()));
                    let _ = std::fs::rename(from, to);
                }
                Self::connect(&db_path).context("failed to create a replacement database")?
            }
        };

        paths::secure_file(&db_path)?;
        Ok(Self {
            conn,
            blobs,
            data_dir: data_dir.to_path_buf(),
            db_path,
        })
    }

    fn connect(db_path: &Path) -> Result<Connection> {
        let mut conn = Connection::open(db_path)
            .with_context(|| format!("failed to open {}", db_path.display()))?;
        schema::apply_pragmas(&conn)?;
        register_functions(&conn)?;
        schema::migrate(&mut conn)?;
        Ok(conn)
    }

    /// In-memory store, for tests.
    #[cfg(test)]
    pub fn open_memory(blob_root: &Path) -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        schema::apply_pragmas(&conn)?;
        register_functions(&conn)?;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn,
            blobs: BlobStore::open(blob_root)?,
            data_dir: blob_root.to_path_buf(),
            db_path: PathBuf::from(":memory:"),
        })
    }

    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn database_path(&self) -> &Path {
        &self.db_path
    }

    // -- writes ---------------------------------------------------------------

    /// Offer a clipping to the history.
    ///
    /// When `collapse_duplicates` is set and the content is already present,
    /// the existing row is moved to the top, its `last_used_at` is refreshed
    /// and its pin state is left untouched (§10). The two statements run in one
    /// transaction so an interrupted write cannot leave the row half-updated.
    pub fn insert(&mut self, clip: &NewClip, collapse_duplicates: bool) -> Result<InsertOutcome> {
        let now = model::now_ms();
        let tx = self.conn.transaction()?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM clipboard_items WHERE content_hash = ?1",
                [&clip.content_hash],
                |r| r.get(0),
            )
            .optional()?;

        let outcome = match existing {
            Some(id) if collapse_duplicates => {
                tx.execute(
                    "UPDATE clipboard_items
                        SET last_used_at = ?1, updated_at = ?1
                      WHERE id = ?2",
                    rusqlite::params![now, id],
                )?;
                InsertOutcome::Duplicate { id }
            }
            // With duplicate collapsing disabled the user has asked for one
            // entry per copy. `content_hash` is UNIQUE, so the row identity is
            // salted with the timestamp; the payload and (for blobs) the blob
            // hash are unchanged, so storage is still de-duplicated.
            Some(_) => {
                let salted = blobs::hash_parts(&[
                    clip.content_hash.as_bytes(),
                    &now.to_le_bytes(),
                    &rand_salt().to_le_bytes(),
                ]);
                let id = Self::insert_row(&tx, clip, &salted, now, now, false)?;
                InsertOutcome::Inserted { id }
            }
            None => {
                let id = Self::insert_row(&tx, clip, &clip.content_hash, now, now, false)?;
                InsertOutcome::Inserted { id }
            }
        };

        tx.commit()?;
        Ok(outcome)
    }

    fn insert_row(
        tx: &Transaction<'_>,
        clip: &NewClip,
        content_hash: &str,
        created_at: i64,
        last_used_at: i64,
        is_pinned: bool,
    ) -> Result<i64> {
        let metadata = serde_json::to_string(&clip.metadata).unwrap_or_else(|_| "{}".to_string());
        tx.execute(
            "INSERT INTO clipboard_items
               (kind, mime_type, content_hash, text_content, blob_hash, preview,
                metadata, size_bytes, created_at, updated_at, last_used_at, is_pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11)",
            rusqlite::params![
                clip.kind.as_str(),
                clip.mime_type,
                content_hash,
                clip.text_content,
                clip.blob_hash,
                clip.preview,
                metadata,
                clip.size_bytes as i64,
                created_at,
                last_used_at,
                is_pinned as i32,
            ],
        )?;
        Ok(tx.last_insert_rowid())
    }

    /// Move an entry to the top of the list without changing its content.
    ///
    /// Called when the user restores an entry to the clipboard, which is how
    /// Windows behaves: re-using an old clipping makes it current.
    pub fn touch(&self, id: i64) -> Result<()> {
        let now = model::now_ms();
        self.conn.execute(
            "UPDATE clipboard_items SET last_used_at = ?1 WHERE id = ?2",
            rusqlite::params![now, id],
        )?;
        Ok(())
    }

    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<()> {
        let now = model::now_ms();
        self.conn.execute(
            "UPDATE clipboard_items SET is_pinned = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![pinned as i32, now, id],
        )?;
        Ok(())
    }

    /// Replace the text of a text/url entry (§13).
    ///
    /// Re-derives the kind, preview and content hash. If the edited text
    /// happens to equal another entry's content the two are merged — the other
    /// row is removed and its pin state is inherited — because leaving two rows
    /// with the same content would violate the UNIQUE constraint and would be
    /// confusing in the list.
    pub fn update_text(&mut self, id: i64, new_text: &str, preview_len: usize) -> Result<()> {
        let Some(item) = self.get(id)? else {
            return Err(anyhow!("history entry {id} no longer exists"));
        };
        if !item.kind.is_editable_text() {
            return Err(anyhow!("{} entries cannot be edited as text", item.kind.label()));
        }

        let kind = crate::clipboard::normalize::classify_text(new_text);
        let mime = item.mime_type.clone();
        let hash = blobs::hash_parts(&[kind.as_str().as_bytes(), mime.as_bytes(), new_text.as_bytes()]);
        let preview = model::truncate_chars(new_text, preview_len);
        let now = model::now_ms();

        // Keep the inline/blob split consistent with how the monitor stores
        // text, so an edit cannot turn a tidy row into a multi-megabyte one.
        let (inline_text, new_blob) = if new_text.len() > INLINE_TEXT_LIMIT {
            (None, Some(self.blobs.put(new_text.as_bytes())?))
        } else {
            (Some(new_text.to_string()), None)
        };
        let old_blob = item.blob_hash.clone();

        let mut metadata = item.metadata.clone();
        metadata.edited = true;
        metadata.line_count = Some(new_text.lines().count().max(1) as u32);
        metadata.char_count = Some(new_text.chars().count() as u32);
        let metadata_json = serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());

        let tx = self.conn.transaction()?;

        // Merge with a colliding row, if any.
        let collision: Option<(i64, i64)> = tx
            .query_row(
                "SELECT id, is_pinned FROM clipboard_items
                  WHERE content_hash = ?1 AND id <> ?2",
                rusqlite::params![hash, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let inherit_pin = match collision {
            Some((other_id, other_pinned)) => {
                tx.execute("DELETE FROM clipboard_items WHERE id = ?1", [other_id])?;
                other_pinned != 0
            }
            None => false,
        };

        tx.execute(
            "UPDATE clipboard_items
                SET kind = ?1, content_hash = ?2, text_content = ?3, blob_hash = ?4,
                    preview = ?5, metadata = ?6, size_bytes = ?7,
                    updated_at = ?8, last_used_at = ?8,
                    is_pinned = CASE WHEN ?9 THEN 1 ELSE is_pinned END
              WHERE id = ?10",
            rusqlite::params![
                kind.as_str(),
                hash,
                inline_text,
                new_blob,
                preview,
                metadata_json,
                new_text.len() as i64,
                now,
                inherit_pin as i32,
                id,
            ],
        )?;
        tx.commit()?;

        // The row no longer points at the old blob; collect it if it has become
        // unreferenced (§48).
        if let Some(old) = old_blob {
            if Some(&old) != new_blob.as_ref() {
                self.collect_blob_if_unreferenced(&old)?;
            }
        }
        Ok(())
    }

    /// Delete one entry and collect its blob if nothing else references it.
    pub fn delete(&mut self, id: i64) -> Result<bool> {
        let blob_hash: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT blob_hash FROM clipboard_items WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(blob_hash) = blob_hash else {
            return Ok(false);
        };

        self.conn
            .execute("DELETE FROM clipboard_items WHERE id = ?1", [id])?;

        if let Some(hash) = blob_hash {
            self.collect_blob_if_unreferenced(&hash)?;
        }
        Ok(true)
    }

    /// Delete history. Pinned entries are kept unless `include_pinned`.
    ///
    /// Returns the number of entries removed.
    pub fn clear(&mut self, include_pinned: bool) -> Result<usize> {
        let removed = if include_pinned {
            self.conn.execute("DELETE FROM clipboard_items", [])?
        } else {
            self.conn
                .execute("DELETE FROM clipboard_items WHERE is_pinned = 0", [])?
        };
        // One sweep is cheaper than a per-row reference check after a bulk
        // delete, and it is the only way to be sure nothing was orphaned.
        let orphans = self.collect_orphan_blobs()?;
        tracing::info!(removed, orphan_blobs = orphans, "history cleared");
        Ok(removed)
    }

    /// Delete a blob only once no row references it (§48).
    fn collect_blob_if_unreferenced(&self, hash: &str) -> Result<bool> {
        let referenced: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM clipboard_items WHERE blob_hash = ?1)",
            [hash],
            |r| r.get(0),
        )?;
        if referenced {
            return Ok(false);
        }
        self.blobs.remove(hash)?;
        Ok(true)
    }

    /// Remove every blob on disk that no row references.
    pub fn collect_orphan_blobs(&self) -> Result<usize> {
        let mut referenced = std::collections::HashSet::new();
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT blob_hash FROM clipboard_items WHERE blob_hash IS NOT NULL")?;
        for hash in stmt.query_map([], |r| r.get::<_, String>(0))? {
            referenced.insert(hash?);
        }

        let mut removed = 0usize;
        for hash in self.blobs.list()? {
            if !referenced.contains(&hash) {
                match self.blobs.remove(&hash) {
                    Ok(()) => removed += 1,
                    Err(err) => tracing::warn!(error = %err, "failed to remove orphan blob"),
                }
            }
        }
        Ok(removed)
    }

    // -- reads ----------------------------------------------------------------

    pub fn get(&self, id: i64) -> Result<Option<ClipItem>> {
        let sql = format!("SELECT {ITEM_COLUMNS} FROM clipboard_items WHERE id = ?1");
        Ok(self
            .conn
            .query_row(&sql, [id], row_to_item)
            .optional()
            .context("failed to read history entry")?)
    }

    pub fn get_by_hash(&self, content_hash: &str) -> Result<Option<ClipItem>> {
        let sql = format!("SELECT {ITEM_COLUMNS} FROM clipboard_items WHERE content_hash = ?1");
        Ok(self
            .conn
            .query_row(&sql, [content_hash], row_to_item)
            .optional()?)
    }

    /// Most recent entries, pinned first (§11).
    pub fn recent(&self, limit: usize) -> Result<Vec<ClipItem>> {
        let sql = format!(
            "SELECT {LIST_COLUMNS} FROM clipboard_items
              ORDER BY is_pinned DESC, last_used_at DESC
              LIMIT ?1"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([limit as i64], row_to_item)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Every pinned entry, newest first.
    pub fn pinned(&self) -> Result<Vec<ClipItem>> {
        let sql = format!(
            "SELECT {LIST_COLUMNS} FROM clipboard_items
              WHERE is_pinned = 1 ORDER BY last_used_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_item)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// The payload to put back on the clipboard.
    ///
    /// `item` must be a fully loaded entry (from [`Store::get`]); rows from a
    /// list query do not carry their text. Use [`Store::payload_of`] when you
    /// only have an id.
    pub fn payload(&self, item: &ClipItem) -> Result<Vec<u8>> {
        if let Some(hash) = &item.blob_hash {
            return self.blobs.get(hash);
        }
        if let Some(text) = &item.text_content {
            return Ok(text.as_bytes().to_vec());
        }
        Err(anyhow!("history entry {} has no stored payload", item.id))
    }

    /// Load an entry by id and return its payload.
    ///
    /// This is the call the UI uses when the user activates a row, precisely
    /// because list rows are preview-only.
    pub fn payload_of(&self, id: i64) -> Result<(ClipItem, Vec<u8>)> {
        let item = self
            .get(id)?
            .ok_or_else(|| anyhow!("history entry {id} no longer exists"))?;
        let payload = self.payload(&item)?;
        Ok((item, payload))
    }

    /// Full text of a text or URL entry, for the view and edit dialogs.
    pub fn full_text(&self, id: i64) -> Result<String> {
        let (item, payload) = self.payload_of(id)?;
        if !item.kind.is_editable_text() {
            return Err(anyhow!("{} entries have no text form", item.kind.label()));
        }
        String::from_utf8(payload).context("stored text is not valid UTF-8")
    }

    /// Search the history (§7).
    ///
    /// See [`crate::search`] for the two-stage strategy. An empty query is the
    /// same as [`Store::recent`].
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<ClipItem>> {
        let query = query.trim();
        if query.is_empty() {
            return self.recent(limit);
        }

        // Over-fetch so re-ranking has something to work with, but keep the
        // candidate set bounded so one keystroke is always cheap.
        let candidate_cap = (limit.saturating_mul(6)).clamp(120, 2_000);

        let mut candidates = match search::build_fts_query(query) {
            Some(expr) => self.fts_candidates(&expr, candidate_cap).unwrap_or_else(|err| {
                // A query FTS5 refuses to parse must degrade to the fallback,
                // not surface an error dialog mid-typing.
                tracing::debug!(error = %err, "full-text query rejected; using substring scan");
                Vec::new()
            }),
            None => Vec::new(),
        };

        if candidates.is_empty() {
            candidates = self.substring_candidates(query, candidate_cap)?;
        }

        let now = model::now_ms();
        let mut scored: Vec<_> = candidates
            .into_iter()
            .filter_map(|item| {
                // FTS5 matches *tokens*, so it is deliberately coarser than
                // what the user typed: it drops punctuation and matches
                // prefixes, which means searching `-i` yields a candidate whose
                // only connection to the query is a word starting with "i".
                // Re-scoring against the literal query filters those out.
                //
                // The exception is an entry whose preview is truncated: there
                // the index saw text we did not, so a preview that fails to
                // match proves nothing and the candidate is kept at the bottom
                // of the ranking instead of being discarded.
                let score = match text_score(query, &item) {
                    Some(score) => score,
                    None if item.preview_covers_content() => return None,
                    None => 0,
                };
                Some((
                    search::rank_key(score, item.is_pinned, item.last_used_at, now),
                    item,
                ))
            })
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(scored.into_iter().take(limit).map(|(_, item)| item).collect())
    }

    fn fts_candidates(&self, expr: &str, limit: usize) -> Result<Vec<ClipItem>> {
        let sql = format!(
            "SELECT {}
               FROM clipboard_items_fts AS f
               JOIN clipboard_items AS i ON i.id = f.rowid
              WHERE f.clipboard_items_fts MATCH ?1
              ORDER BY bm25(clipboard_items_fts, 4.0, 2.0, 1.0)
              LIMIT ?2",
            LIST_COLUMNS
                .split(", ")
                .map(|c| {
                    // `NULL` is a literal, not a column, so it must not be
                    // qualified with the table alias.
                    if c.trim() == "NULL" {
                        "NULL".to_string()
                    } else {
                        format!("i.{}", c.trim())
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(rusqlite::params![expr, limit as i64], row_to_item)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Unicode-aware substring scan. Only reached when FTS found nothing.
    fn substring_candidates(&self, query: &str, limit: usize) -> Result<Vec<ClipItem>> {
        let sql = format!(
            "SELECT {LIST_COLUMNS} FROM clipboard_items
              WHERE cm_contains_ci(preview, ?1)
                 OR cm_contains_ci(text_content, ?1)
                 OR cm_contains_ci(metadata, ?1)
              ORDER BY is_pinned DESC, last_used_at DESC
              LIMIT ?2"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(rusqlite::params![query, limit as i64], row_to_item)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn stats(&self) -> Result<Stats> {
        let (total_items, pinned_items): (i64, i64) = self.conn.query_row(
            "SELECT count(*), coalesce(sum(is_pinned), 0) FROM clipboard_items",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        // Include the WAL: a freshly written history can have most of its bytes
        // there, and reporting only the main file would understate usage.
        let mut database_bytes = 0u64;
        for suffix in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{suffix}", self.db_path.display()));
            database_bytes += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        }

        let blob_hashes = self.blobs.list().unwrap_or_default();
        let blob_bytes = blob_hashes
            .iter()
            .filter_map(|h| self.blobs.size_of(h))
            .sum();

        Ok(Stats {
            total_items: total_items as u64,
            pinned_items: pinned_items as u64,
            database_bytes,
            blob_bytes,
            blob_count: blob_hashes.len() as u64,
        })
    }

    /// Direct connection access for maintenance and import/export, which live
    /// in sibling modules.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// Best fuzzy score across everything a list row carries, or `None` when the
/// query matches neither the preview nor the metadata.
///
/// Scored against the preview and the metadata only. The *full* text has
/// already been consulted by the FTS stage, which is what selected this
/// candidate; re-scoring megabytes of text here would undo the bound that
/// makes per-keystroke search cheap. `None` therefore means "no match in what
/// this row carries", not "no match at all" — see [`Store::search`] for how
/// the two are told apart.
fn text_score(query: &str, item: &ClipItem) -> Option<i32> {
    let preview = search::fuzzy_score(query, &item.preview);
    let meta = item.metadata.search_text();
    let metadata = if meta.is_empty() {
        None
    } else {
        search::fuzzy_score(query, &meta)
    };
    match (preview, metadata) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Map a result row onto a [`ClipItem`].
///
/// Deliberately lenient: an unparseable `metadata` column or an unknown `kind`
/// yields a usable item with a logged warning instead of poisoning the whole
/// query, so one bad row cannot empty the user's history list (§35).
fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClipItem> {
    let id: i64 = row.get(0)?;
    let kind_raw: String = row.get(1)?;
    let kind = ClipKind::parse(&kind_raw).unwrap_or_else(|| {
        tracing::warn!(id, kind = %kind_raw, "unknown history entry kind; treating as text");
        ClipKind::Text
    });
    let metadata_raw: String = row.get(7)?;
    let metadata = serde_json::from_str::<Metadata>(&metadata_raw).unwrap_or_else(|err| {
        tracing::warn!(id, error = %err, "unreadable entry metadata; ignoring it");
        Metadata::default()
    });

    Ok(ClipItem {
        id,
        kind,
        mime_type: row.get(2)?,
        content_hash: row.get(3)?,
        text_content: row.get(4)?,
        blob_hash: row.get(5)?,
        preview: row.get(6)?,
        metadata,
        size_bytes: row.get::<_, i64>(8)?.max(0) as u64,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        last_used_at: row.get(11)?,
        is_pinned: row.get::<_, i64>(12)? != 0,
    })
}

/// Register the Unicode-aware helpers SQLite lacks.
fn register_functions(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "cm_contains_ci",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let haystack = ctx.get_raw(0).as_str_or_null()?;
            let needle = ctx.get_raw(1).as_str_or_null()?;
            Ok(match (haystack, needle) {
                (Some(h), Some(n)) => search::contains_ci(h, n),
                // A NULL haystack simply does not match; a NULL needle matches
                // nothing rather than everything, which is the safer default
                // for a filter.
                _ => false,
            })
        },
    )
    .context("failed to register cm_contains_ci")?;
    Ok(())
}

/// Small non-cryptographic salt, used only to make the "one row per copy" mode
/// collision-proof when two copies land in the same millisecond.
fn rand_salt() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClipKind, FileMeta, ImageMeta};

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_memory(&tmp.path().join("blobs")).unwrap();
        (tmp, store)
    }

    fn text_clip(text: &str) -> NewClip {
        let kind = crate::clipboard::normalize::classify_text(text);
        NewClip {
            kind,
            mime_type: "text/plain;charset=utf-8".to_string(),
            content_hash: blobs::hash_parts(&[
                kind.as_str().as_bytes(),
                b"text/plain;charset=utf-8",
                text.as_bytes(),
            ]),
            text_content: Some(text.to_string()),
            blob_hash: None,
            preview: model::truncate_chars(text, 220),
            metadata: Metadata {
                line_count: Some(text.lines().count().max(1) as u32),
                char_count: Some(text.chars().count() as u32),
                ..Default::default()
            },
            size_bytes: text.len() as u64,
        }
    }

    fn insert_text(store: &mut Store, text: &str) -> i64 {
        store.insert(&text_clip(text), true).unwrap().id()
    }

    // -- insert / duplicates --------------------------------------------------

    #[test]
    fn insert_and_retrieve() {
        let (_t, mut store) = store();
        let id = insert_text(&mut store, "Hello World");
        let item = store.get(id).unwrap().unwrap();
        assert_eq!(item.text_content.as_deref(), Some("Hello World"));
        assert_eq!(item.kind, ClipKind::Text);
        assert!(!item.is_pinned);
        assert_eq!(store.payload(&item).unwrap(), b"Hello World");
    }

    #[test]
    fn repeated_copies_do_not_create_duplicates() {
        let (_t, mut store) = store();
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(store.insert(&text_clip("Hello"), true).unwrap());
        }
        assert!(ids[0].is_new());
        assert!(ids[1..].iter().all(|o| !o.is_new()));
        assert!(ids.iter().all(|o| o.id() == ids[0].id()));
        assert_eq!(store.stats().unwrap().total_items, 1);
    }

    #[test]
    fn duplicate_moves_to_top_and_keeps_its_pin() {
        let (_t, mut store) = store();
        let first = insert_text(&mut store, "first");
        store.set_pinned(first, true).unwrap();
        insert_text(&mut store, "second");
        insert_text(&mut store, "third");

        std::thread::sleep(std::time::Duration::from_millis(5));
        let again = store.insert(&text_clip("first"), true).unwrap();
        assert_eq!(again, InsertOutcome::Duplicate { id: first });

        let item = store.get(first).unwrap().unwrap();
        assert!(item.is_pinned, "pin state must survive a duplicate copy");

        // Among unpinned rows, "first" would now be newest; it is pinned so it
        // heads the list either way.
        let recent = store.recent(10).unwrap();
        assert_eq!(recent[0].id, first);
    }

    #[test]
    fn duplicate_collapsing_can_be_disabled() {
        let (_t, mut store) = store();
        store.insert(&text_clip("Hello"), false).unwrap();
        store.insert(&text_clip("Hello"), false).unwrap();
        store.insert(&text_clip("Hello"), false).unwrap();
        assert_eq!(store.stats().unwrap().total_items, 3);
    }

    #[test]
    fn multiline_unicode_and_emoji_survive_a_round_trip() {
        let (_t, mut store) = store();
        let payload = "line one\n\tindented\n\nпривет 日本語 👍🏽 café\r\nlast";
        let id = insert_text(&mut store, payload);
        let item = store.get(id).unwrap().unwrap();
        assert_eq!(item.text_content.as_deref(), Some(payload));
        assert_eq!(store.payload(&item).unwrap(), payload.as_bytes());
    }

    // -- ordering -------------------------------------------------------------

    #[test]
    fn newest_first_with_pinned_above() {
        let (_t, mut store) = store();
        for text in ["one", "two", "three", "four"] {
            insert_text(&mut store, text);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let oldest = store.get_by_hash(&text_clip("one").content_hash).unwrap().unwrap();
        store.set_pinned(oldest.id, true).unwrap();

        let order: Vec<_> = store
            .recent(10)
            .unwrap()
            .iter()
            .map(|i| i.preview.clone())
            .collect();
        assert_eq!(order, vec!["one", "four", "three", "two"]);
    }

    // -- pinning --------------------------------------------------------------

    #[test]
    fn pin_and_unpin() {
        let (_t, mut store) = store();
        let id = insert_text(&mut store, "pin me");
        store.set_pinned(id, true).unwrap();
        assert!(store.get(id).unwrap().unwrap().is_pinned);
        assert_eq!(store.pinned().unwrap().len(), 1);
        store.set_pinned(id, false).unwrap();
        assert!(!store.get(id).unwrap().unwrap().is_pinned);
        assert!(store.pinned().unwrap().is_empty());
    }

    #[test]
    fn clear_keeps_pinned_unless_asked() {
        let (_t, mut store) = store();
        let keep = insert_text(&mut store, "keep me");
        insert_text(&mut store, "drop me");
        insert_text(&mut store, "drop me too");
        store.set_pinned(keep, true).unwrap();

        assert_eq!(store.clear(false).unwrap(), 2);
        assert_eq!(store.stats().unwrap().total_items, 1);
        assert!(store.get(keep).unwrap().is_some());

        assert_eq!(store.clear(true).unwrap(), 1);
        assert_eq!(store.stats().unwrap().total_items, 0);
    }

    // -- delete ---------------------------------------------------------------

    #[test]
    fn delete_removes_one_entry() {
        let (_t, mut store) = store();
        let a = insert_text(&mut store, "a");
        let b = insert_text(&mut store, "b");
        assert!(store.delete(a).unwrap());
        assert!(!store.delete(a).unwrap(), "second delete is a no-op");
        assert!(store.get(a).unwrap().is_none());
        assert!(store.get(b).unwrap().is_some());
    }

    // -- blobs ----------------------------------------------------------------

    fn image_clip(store: &Store, bytes: &[u8]) -> NewClip {
        let blob_hash = store.blobs().put(bytes).unwrap();
        NewClip {
            kind: ClipKind::Image,
            mime_type: "image/png".into(),
            content_hash: blobs::hash_parts(&[b"image", b"image/png", bytes]),
            text_content: None,
            blob_hash: Some(blob_hash),
            preview: "Image 2 × 2".into(),
            metadata: Metadata {
                image: Some(ImageMeta { width: 2, height: 2, format: "png".into() }),
                ..Default::default()
            },
            size_bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn image_payload_round_trips_through_the_blob_store() {
        let (_t, mut store) = store();
        let bytes = b"\x89PNG\r\n\x1a\nfake image data".to_vec();
        let clip = image_clip(&store, &bytes);
        let id = store.insert(&clip, true).unwrap().id();

        let item = store.get(id).unwrap().unwrap();
        assert_eq!(item.kind, ClipKind::Image);
        assert_eq!(store.payload(&item).unwrap(), bytes);
    }

    #[test]
    fn deleting_an_image_collects_its_blob() {
        let (_t, mut store) = store();
        let clip = image_clip(&store, b"image-bytes");
        let hash = clip.blob_hash.clone().unwrap();
        let id = store.insert(&clip, true).unwrap().id();

        assert!(store.blobs().exists(&hash));
        store.delete(id).unwrap();
        assert!(!store.blobs().exists(&hash), "orphan blob should be collected");
    }

    #[test]
    fn a_blob_shared_with_a_pinned_entry_is_not_collected() {
        // Two entries differing only in MIME type share one blob. Deleting one
        // must not break the other — this is the §48 requirement.
        let (_t, mut store) = store();
        let bytes = b"shared-image-bytes";
        let blob_hash = store.blobs().put(bytes).unwrap();

        let make = |mime: &str| NewClip {
            kind: ClipKind::Image,
            mime_type: mime.to_string(),
            content_hash: blobs::hash_parts(&[b"image", mime.as_bytes(), bytes]),
            text_content: None,
            blob_hash: Some(blob_hash.clone()),
            preview: "Image".into(),
            metadata: Metadata::default(),
            size_bytes: bytes.len() as u64,
        };
        let a = store.insert(&make("image/png"), true).unwrap().id();
        let b = store.insert(&make("image/webp"), true).unwrap().id();
        store.set_pinned(b, true).unwrap();

        store.delete(a).unwrap();
        assert!(store.blobs().exists(&blob_hash), "blob is still referenced");
        let kept = store.get(b).unwrap().unwrap();
        assert_eq!(store.payload(&kept).unwrap(), bytes);

        // Clearing unpinned entries must also leave it alone.
        store.clear(false).unwrap();
        assert!(store.blobs().exists(&blob_hash));
    }

    #[test]
    fn clear_all_collects_every_blob() {
        let (_t, mut store) = store();
        let a = image_clip(&store, b"a");
        let b = image_clip(&store, b"b");
        store.insert(&a, true).unwrap();
        store.insert(&b, true).unwrap();
        assert_eq!(store.blobs().list().unwrap().len(), 2);
        store.clear(true).unwrap();
        assert!(store.blobs().list().unwrap().is_empty());
    }

    #[test]
    fn missing_blob_is_an_error_not_a_panic() {
        let (_t, mut store) = store();
        let clip = image_clip(&store, b"will vanish");
        let id = store.insert(&clip, true).unwrap().id();
        store.blobs().remove(clip.blob_hash.as_ref().unwrap()).unwrap();

        let item = store.get(id).unwrap().unwrap();
        assert!(store.payload(&item).is_err());
        // The entry itself is still listable, so the UI can offer to remove it.
        assert_eq!(store.recent(10).unwrap().len(), 1);
    }

    // -- editing --------------------------------------------------------------

    #[test]
    fn editing_text_updates_content_and_hash() {
        let (_t, mut store) = store();
        let id = insert_text(&mut store, "sudo systemctl restart nginx");
        let before = store.get(id).unwrap().unwrap();

        store
            .update_text(id, "sudo systemctl restart apache2", 220)
            .unwrap();

        let after = store.get(id).unwrap().unwrap();
        assert_eq!(after.text_content.as_deref(), Some("sudo systemctl restart apache2"));
        assert_eq!(after.preview, "sudo systemctl restart apache2");
        assert_ne!(after.content_hash, before.content_hash);
        assert!(after.metadata.edited);
        assert_eq!(store.payload(&after).unwrap(), b"sudo systemctl restart apache2");
    }

    #[test]
    fn editing_text_into_a_url_reclassifies_it() {
        let (_t, mut store) = store();
        let id = insert_text(&mut store, "not a link");
        store.update_text(id, "https://github.com/example", 220).unwrap();
        assert_eq!(store.get(id).unwrap().unwrap().kind, ClipKind::Url);
    }

    #[test]
    fn editing_into_an_existing_entry_merges_them() {
        let (_t, mut store) = store();
        let target = insert_text(&mut store, "aaa");
        let other = insert_text(&mut store, "bbb");
        store.set_pinned(other, true).unwrap();

        store.update_text(target, "bbb", 220).unwrap();

        assert!(store.get(other).unwrap().is_none(), "collision row is removed");
        let merged = store.get(target).unwrap().unwrap();
        assert_eq!(merged.text_content.as_deref(), Some("bbb"));
        assert!(merged.is_pinned, "pin state should be inherited");
        assert_eq!(store.stats().unwrap().total_items, 1);
    }

    #[test]
    fn images_cannot_be_edited_as_text() {
        let (_t, mut store) = store();
        let clip = image_clip(&store, b"img");
        let id = store.insert(&clip, true).unwrap().id();
        assert!(store.update_text(id, "nope", 220).is_err());
    }

    #[test]
    fn editing_a_missing_entry_errors() {
        let (_t, mut store) = store();
        assert!(store.update_text(4242, "whatever", 220).is_err());
    }

    // -- search ---------------------------------------------------------------

    fn seed_search_history(store: &mut Store) {
        for text in [
            "sudo systemctl restart nginx",
            "kubectl get pods",
            "ffmpeg -i input.mp4 output.ts",
            "AWS MediaConnect flow",
            "Cloudport playlist issue",
        ] {
            insert_text(store, text);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn the_acceptance_search_works() {
        // Straight from §7.
        let (_t, mut store) = store();
        seed_search_history(&mut store);
        let hits = store.search("ffmpeg", 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].preview, "ffmpeg -i input.mp4 output.ts");
    }

    #[test]
    fn search_is_case_insensitive_and_partial() {
        let (_t, mut store) = store();
        seed_search_history(&mut store);
        for q in ["NGINX", "nginx", "ngi", "SYSTEMCTL"] {
            let hits = store.search(q, 20).unwrap();
            assert!(
                hits.iter().any(|h| h.preview.contains("nginx")),
                "query {q:?} found {:?}",
                hits.iter().map(|h| &h.preview).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn search_finds_matches_inside_a_word_via_the_fallback() {
        let (_t, mut store) = store();
        seed_search_history(&mut store);
        // "fmpeg" is not a token prefix, so only the substring fallback can
        // find it.
        let hits = store.search("fmpeg", 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].preview, "ffmpeg -i input.mp4 output.ts");
    }

    #[test]
    fn search_handles_unicode_and_diacritics() {
        let (_t, mut store) = store();
        insert_text(&mut store, "Beyoncé café au lait");
        insert_text(&mut store, "日本語のテキスト");
        insert_text(&mut store, "Grüße aus Köln");

        assert_eq!(store.search("café", 10).unwrap().len(), 1);
        assert_eq!(store.search("cafe", 10).unwrap().len(), 1, "diacritics folded");
        assert_eq!(store.search("日本語", 10).unwrap().len(), 1);
        assert_eq!(store.search("grüße", 10).unwrap().len(), 1);
        assert_eq!(store.search("GRÜSSE", 10).unwrap().len(), 0, "ß is not folded to ss");
    }

    #[test]
    fn empty_search_returns_recent_history() {
        let (_t, mut store) = store();
        seed_search_history(&mut store);
        assert_eq!(store.search("", 20).unwrap().len(), 5);
        assert_eq!(store.search("   ", 20).unwrap().len(), 5);
    }

    #[test]
    fn search_matches_urls() {
        let (_t, mut store) = store();
        insert_text(&mut store, "https://github.com/example/project");
        insert_text(&mut store, "https://example.com");
        assert_eq!(store.search("github", 10).unwrap().len(), 1);
        assert_eq!(store.search("https", 10).unwrap().len(), 2);
        assert_eq!(store.search("example", 10).unwrap().len(), 2);
    }

    #[test]
    fn search_matches_file_names_in_metadata() {
        let (_t, mut store) = store();
        let bytes = b"file:///home/user/Documents/quarterly-report.pdf\r\n";
        let blob_hash = store.blobs().put(bytes).unwrap();
        let clip = NewClip {
            kind: ClipKind::Files,
            mime_type: "text/uri-list".into(),
            content_hash: blobs::hash_parts(&[b"files", b"text/uri-list", bytes]),
            text_content: None,
            blob_hash: Some(blob_hash),
            preview: "quarterly-report.pdf".into(),
            metadata: Metadata {
                files: vec![FileMeta {
                    path: "/home/user/Documents/quarterly-report.pdf".into(),
                    name: "quarterly-report.pdf".into(),
                    size: Some(10),
                    is_dir: false,
                }],
                ..Default::default()
            },
            size_bytes: bytes.len() as u64,
        };
        store.insert(&clip, true).unwrap();

        assert_eq!(store.search("quarterly", 10).unwrap().len(), 1);
        assert_eq!(store.search("Documents", 10).unwrap().len(), 1);
        assert_eq!(store.search("pdf", 10).unwrap().len(), 1);
    }

    #[test]
    fn search_respects_the_limit_and_puts_pinned_first() {
        let (_t, mut store) = store();
        for i in 0..40 {
            insert_text(&mut store, &format!("entry number {i} nginx"));
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let oldest = store
            .get_by_hash(&text_clip("entry number 0 nginx").content_hash)
            .unwrap()
            .unwrap();
        store.set_pinned(oldest.id, true).unwrap();

        let hits = store.search("nginx", 5).unwrap();
        assert_eq!(hits.len(), 5);
        assert_eq!(hits[0].id, oldest.id, "pinned match must come first");
    }

    #[test]
    fn search_with_only_punctuation_does_not_error() {
        let (_t, mut store) = store();
        seed_search_history(&mut store);
        // `-i` tokenises to the prefix term `"i"*`, which also matches the
        // word "issue". Re-scoring against the literal query must discard that.
        let hits = store.search("-i", 10).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.preview.as_str()).collect::<Vec<_>>(),
            vec!["ffmpeg -i input.mp4 output.ts"]
        );
        assert!(store.search("@@@@", 10).unwrap().is_empty());
    }

    #[test]
    fn a_match_beyond_the_truncated_preview_is_still_found() {
        // The counterpart to the test above: a candidate whose preview does not
        // contain the query must not be discarded when the preview is only part
        // of the entry, because the FTS index saw text the row does not carry.
        let (_t, mut store) = store();
        let long = format!("{}\nthe needle is pelicans", "filler ".repeat(200));
        let id = insert_text(&mut store, &long);
        insert_text(&mut store, "an unrelated entry");

        let item = store.get(id).unwrap().unwrap();
        assert!(!item.preview.contains("pelicans"), "preview must be truncated");
        assert!(!item.preview_covers_content());

        let hits = store.search("pelicans", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
    }

    #[test]
    fn search_survives_fts_operator_injection() {
        let (_t, mut store) = store();
        seed_search_history(&mut store);
        for q in ["NEAR(a b)", "\" OR \"", "nginx*", "a AND OR NOT", "^"] {
            // Must not error, whatever it returns.
            store.search(q, 10).unwrap();
        }
    }

    #[test]
    fn search_scales_to_a_large_history() {
        let (_t, mut store) = store();
        for i in 0..5_000 {
            // Avoid the per-insert sleep used elsewhere; ordering is not under
            // test here.
            insert_text(&mut store, &format!("filler entry {i}"));
        }
        insert_text(&mut store, "the needle is ffmpeg -i a.mp4 b.ts");

        let start = std::time::Instant::now();
        let hits = store.search("ffmpeg", 20).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(hits.len(), 1);
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "indexed search took {elapsed:?} over 5000 entries"
        );
    }

    // -- stats ----------------------------------------------------------------

    #[test]
    fn stats_count_items_and_blobs() {
        let (_t, mut store) = store();
        insert_text(&mut store, "text");
        let clip = image_clip(&store, &vec![7u8; 1000]);
        let img = store.insert(&clip, true).unwrap().id();
        store.set_pinned(img, true).unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.total_items, 2);
        assert_eq!(stats.pinned_items, 1);
        assert_eq!(stats.blob_count, 1);
        assert_eq!(stats.blob_bytes, 1000);
    }

    // -- durability -----------------------------------------------------------

    #[test]
    fn history_survives_reopening_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");

        let (pinned_id, img_hash) = {
            let mut store = Store::open_at(&dir).unwrap();
            insert_text(&mut store, "transient");
            let id = insert_text(&mut store, "survivor");
            store.set_pinned(id, true).unwrap();
            let clip = image_clip(&store, b"persisted image");
            store.insert(&clip, true).unwrap();
            (id, clip.blob_hash.unwrap())
        };

        let store = Store::open_at(&dir).unwrap();
        assert_eq!(store.stats().unwrap().total_items, 3);
        let item = store.get(pinned_id).unwrap().unwrap();
        assert!(item.is_pinned);
        assert_eq!(item.text_content.as_deref(), Some("survivor"));
        assert_eq!(store.blobs().get(&img_hash).unwrap(), b"persisted image");
        assert_eq!(store.search("survivor", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_corrupt_database_is_quarantined_and_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        std::fs::create_dir_all(&dir).unwrap();
        // Not a SQLite file at all.
        std::fs::write(dir.join("database.sqlite"), b"\x00\x01garbage not sqlite").unwrap();

        let mut store = Store::open_at(&dir).expect("should recover rather than fail");
        insert_text(&mut store, "works again");
        assert_eq!(store.stats().unwrap().total_items, 1);

        let quarantined = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("corrupt-"));
        assert!(quarantined, "the unusable file should be kept for support");
    }

    #[test]
    fn database_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        let store = Store::open_at(&dir).unwrap();

        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let db_mode = std::fs::metadata(store.database_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(db_mode, 0o600);
    }
}
