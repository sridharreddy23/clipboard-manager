//! Database schema and forward-only migrations.
//!
//! The schema version is stored in SQLite's own `user_version` pragma, which
//! avoids a bootstrap table and is readable with `sqlite3 database.sqlite
//! 'PRAGMA user_version'` for support purposes.
//!
//! Migrations run inside a single transaction, so an interrupted upgrade leaves
//! the database at the previous version rather than half-migrated (§48).

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

/// Schema version this build expects.
pub const SCHEMA_VERSION: i32 = 1;

/// Connection-level pragmas.
///
/// * `journal_mode=WAL` — readers never block the writer, and a crash mid-write
///   is rolled back from the journal on next open (§49).
/// * `synchronous=NORMAL` — with WAL this is crash-safe against process death
///   and OS crash; only a power loss can lose the last transaction, which for
///   clipboard history is an acceptable trade for not fsync-ing on every copy.
/// * `busy_timeout` — a second process (e.g. `clipboard-manager export`
///   running while the daemon writes) waits rather than failing (§35).
/// * `foreign_keys` — enforced for future schema additions.
pub fn apply_pragmas(conn: &Connection) -> Result<()> {
    // `journal_mode` returns a row, so it needs `query_row`, not `execute`.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .context("failed to enable WAL")?;
    if !mode.eq_ignore_ascii_case("wal") {
        // Happens on filesystems without the required locking (some network
        // mounts). Not fatal; we simply run slower and with a rollback journal.
        tracing::warn!(mode = %mode, "WAL unavailable; falling back to the default journal");
    }
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;
         -- Cap the page cache at ~8 MiB so a large history cannot inflate the
         -- resident set of a background daemon (§45).
         PRAGMA cache_size = -8192;",
    )
    .context("failed to apply pragmas")?;
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Create or upgrade the schema. Idempotent.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let from = user_version(conn)?;

    if from > SCHEMA_VERSION {
        bail!(
            "database schema version {from} is newer than this build supports \
             ({SCHEMA_VERSION}); refusing to downgrade"
        );
    }
    if from == SCHEMA_VERSION {
        return Ok(());
    }

    let tx = conn.transaction()?;
    if from < 1 {
        tx.execute_batch(V1).context("failed to create schema v1")?;
    }
    // Future migrations append here:
    //   if from < 2 { tx.execute_batch(V2)?; }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit().context("failed to commit migration")?;

    tracing::info!(from, to = SCHEMA_VERSION, "database schema initialised");
    Ok(())
}

const V1: &str = r#"
CREATE TABLE clipboard_items (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT    NOT NULL,
    mime_type     TEXT    NOT NULL,
    -- sha256(kind || mime || payload). UNIQUE is what makes duplicate
    -- suppression an atomic upsert instead of a read-then-write race (§10).
    content_hash  TEXT    NOT NULL UNIQUE,
    -- Inline payload for text/url. NULL for blob-backed kinds.
    text_content  TEXT,
    -- Pointer into blobs/<hash> for image/files. NULL for text kinds.
    blob_hash     TEXT,
    -- What the list renders; always populated, always searchable.
    preview       TEXT    NOT NULL,
    metadata      TEXT    NOT NULL DEFAULT '{}',
    size_bytes    INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL,
    is_pinned     INTEGER NOT NULL DEFAULT 0 CHECK (is_pinned IN (0, 1))
);

-- The popup's default query is exactly "pinned first, then most recent", so
-- this index lets it be answered from the index alone.
CREATE INDEX idx_items_order    ON clipboard_items (is_pinned DESC, last_used_at DESC);
-- Retention sweeps scan by age among unpinned rows only.
CREATE INDEX idx_items_age      ON clipboard_items (is_pinned, last_used_at);
-- Blob garbage collection asks "is this hash still referenced?".
CREATE INDEX idx_items_blob     ON clipboard_items (blob_hash) WHERE blob_hash IS NOT NULL;

-- Full-text index. `content=` makes this an external-content table: the text
-- is not duplicated, FTS5 reads it from clipboard_items when it needs it.
-- `metadata` is indexed too, so file names inside a file selection and the
-- source application name are searchable (§7).
CREATE VIRTUAL TABLE clipboard_items_fts USING fts5 (
    preview,
    text_content,
    metadata,
    content      = 'clipboard_items',
    content_rowid= 'id',
    tokenize     = "unicode61 remove_diacritics 2"
);

CREATE TRIGGER clipboard_items_fts_ai AFTER INSERT ON clipboard_items BEGIN
    INSERT INTO clipboard_items_fts (rowid, preview, text_content, metadata)
    VALUES (new.id, new.preview, new.text_content, new.metadata);
END;

CREATE TRIGGER clipboard_items_fts_ad AFTER DELETE ON clipboard_items BEGIN
    INSERT INTO clipboard_items_fts (clipboard_items_fts, rowid, preview, text_content, metadata)
    VALUES ('delete', old.id, old.preview, old.text_content, old.metadata);
END;

-- Scoped to the indexed columns: bumping last_used_at on a duplicate copy must
-- not re-index the row (§45).
CREATE TRIGGER clipboard_items_fts_au
AFTER UPDATE OF preview, text_content, metadata ON clipboard_items BEGIN
    INSERT INTO clipboard_items_fts (clipboard_items_fts, rowid, preview, text_content, metadata)
    VALUES ('delete', old.id, old.preview, old.text_content, old.metadata);
    INSERT INTO clipboard_items_fts (rowid, preview, text_content, metadata)
    VALUES (new.id, new.preview, new.text_content, new.metadata);
END;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    #[test]
    fn migration_creates_schema_and_sets_version() {
        let conn = fresh();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE name IN ('clipboard_items', 'clipboard_items_fts')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }

    #[test]
    fn migration_is_idempotent() {
        let mut conn = fresh();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_is_refused_rather_than_corrupted() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
            .unwrap();
        let err = migrate(&mut conn).unwrap_err().to_string();
        assert!(err.contains("newer than this build"), "{err}");
    }

    #[test]
    fn fts_triggers_track_inserts_updates_and_deletes() {
        let conn = fresh();
        conn.execute(
            "INSERT INTO clipboard_items
               (kind, mime_type, content_hash, text_content, preview, metadata,
                size_bytes, created_at, updated_at, last_used_at)
             VALUES ('text','text/plain','h1','ffmpeg -i input.mp4 output.ts',
                     'ffmpeg -i input.mp4 output.ts','{}',29,1,1,1)",
            [],
        )
        .unwrap();

        let hits = |q: &str, conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM clipboard_items_fts WHERE clipboard_items_fts MATCH ?1",
                [q],
                |r| r.get(0),
            )
            .unwrap()
        };

        assert_eq!(hits("ffmpeg", &conn), 1);

        conn.execute(
            "UPDATE clipboard_items SET preview = 'kubectl get pods',
                                        text_content = 'kubectl get pods' WHERE id = 1",
            [],
        )
        .unwrap();
        assert_eq!(hits("ffmpeg", &conn), 0);
        assert_eq!(hits("kubectl", &conn), 1);

        conn.execute("DELETE FROM clipboard_items WHERE id = 1", []).unwrap();
        assert_eq!(hits("kubectl", &conn), 0);
    }

    #[test]
    fn touching_last_used_does_not_disturb_the_index() {
        let conn = fresh();
        conn.execute(
            "INSERT INTO clipboard_items
               (kind, mime_type, content_hash, text_content, preview, metadata,
                size_bytes, created_at, updated_at, last_used_at)
             VALUES ('text','text/plain','h1','nginx','nginx','{}',5,1,1,1)",
            [],
        )
        .unwrap();
        conn.execute("UPDATE clipboard_items SET last_used_at = 99 WHERE id = 1", [])
            .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM clipboard_items_fts WHERE clipboard_items_fts MATCH 'nginx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "row should still be indexed exactly once");
    }

    #[test]
    fn diacritics_are_folded_by_the_tokenizer() {
        let conn = fresh();
        conn.execute(
            "INSERT INTO clipboard_items
               (kind, mime_type, content_hash, text_content, preview, metadata,
                size_bytes, created_at, updated_at, last_used_at)
             VALUES ('text','text/plain','h1','Beyoncé café','Beyoncé café','{}',12,1,1,1)",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM clipboard_items_fts WHERE clipboard_items_fts MATCH 'cafe'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn content_hash_is_unique() {
        let conn = fresh();
        let insert = |h: &str| {
            conn.execute(
                "INSERT INTO clipboard_items
                   (kind, mime_type, content_hash, preview, metadata,
                    size_bytes, created_at, updated_at, last_used_at)
                 VALUES ('text','text/plain',?1,'p','{}',1,1,1,1)",
                [h],
            )
        };
        insert("dup").unwrap();
        assert!(insert("dup").is_err());
    }
}
