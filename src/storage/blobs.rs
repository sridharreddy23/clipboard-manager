//! Content-addressed blob store for image and file-list payloads (§15).
//!
//! Large payloads are deliberately kept **out** of SQLite. A 50 MB screenshot
//! stored as a BLOB would be copied through the page cache on every read,
//! would bloat the WAL, and would make `VACUUM` expensive. Instead the row
//! holds a SHA-256 and the bytes live in a file.
//!
//! Layout — a two-character fan-out directory keeps any single directory from
//! growing to 100 000 entries, which matters for `readdir` performance and for
//! filesystems with per-directory limits:
//!
//! ```text
//! blobs/
//!   a3/
//!     a3f1…c9        (mode 0600)
//!   7d/
//!     7d02…41
//! ```
//!
//! Because the name *is* the hash, identical payloads are automatically
//! de-duplicated across history entries, and a write is idempotent.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::paths;

/// Hex-encoded SHA-256 of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_lower(&digest)
}

/// Hex-encoded SHA-256 over several byte slices, hashed in order.
///
/// Used for the item content hash, which covers the kind and MIME type as well
/// as the payload so that the same bytes offered as two different types are two
/// different entries.
pub fn hash_parts(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        // Length-prefix each part so that ("ab","c") and ("a","bc") hash
        // differently.
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// True for a well-formed lowercase hex SHA-256.
///
/// Validated before a hash from the database (or from an imported JSON file) is
/// turned into a path, so a hostile or corrupt value cannot escape the blob
/// directory via `../`.
fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (creating if needed) a blob store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        paths::ensure_private_dir(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, hash: &str) -> Result<PathBuf> {
        if !is_valid_hash(hash) {
            bail!("refusing to use malformed blob hash");
        }
        Ok(self.root.join(&hash[..2]).join(hash))
    }

    /// Store `bytes`, returning their hash.
    ///
    /// Idempotent: if the blob already exists with the right size it is left
    /// alone. A pre-existing file with the wrong size can only mean a truncated
    /// earlier write, so it is rewritten.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let hash = hash_bytes(bytes);
        let path = self.path_for(&hash)?;

        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() == bytes.len() as u64 {
                return Ok(hash);
            }
            tracing::warn!(
                hash = %crate::model::short_hash(&hash),
                "blob exists with unexpected size; rewriting"
            );
        }

        let dir = path.parent().expect("blob path always has a parent");
        paths::ensure_private_dir(dir)?;

        // Write to a temporary name in the same directory, fsync, then rename.
        // A reader therefore never observes a partially written blob, which is
        // what makes crash recovery trivial (§49).
        let tmp = dir.join(format!(".{hash}.tmp.{}", std::process::id()));
        let write = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(paths::FILE_MODE)
                .open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &path)?;
            Ok(())
        })();

        if write.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write.with_context(|| format!("failed to store blob in {}", dir.display()))?;
        Ok(hash)
    }

    /// Read a blob back.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.path_for(hash)?;
        fs::read(&path).with_context(|| {
            format!(
                "blob {} is missing from the store",
                crate::model::short_hash(hash)
            )
        })
    }

    pub fn exists(&self, hash: &str) -> bool {
        self.path_for(hash).map(|p| p.exists()).unwrap_or(false)
    }

    pub fn size_of(&self, hash: &str) -> Option<u64> {
        self.path_for(hash)
            .ok()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len())
    }

    /// Delete a blob. A missing blob is not an error — garbage collection is
    /// allowed to race with itself.
    pub fn remove(&self, hash: &str) -> Result<()> {
        let path = self.path_for(hash)?;
        match fs::remove_file(&path) {
            Ok(()) => {
                // Remove the fan-out directory too once it is empty, so an
                // emptied store does not leave 256 stale directories behind.
                if let Some(dir) = path.parent() {
                    let _ = fs::remove_dir(dir);
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to delete {}", path.display())),
        }
    }

    /// Every hash currently present on disk.
    ///
    /// Ignores anything that is not a valid hash filename, which skips the
    /// `.tmp` files of an interrupted write.
    pub fn list(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let outer = match fs::read_dir(&self.root) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).context("failed to list blob store"),
        };
        for entry in outer.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let inner = match fs::read_dir(entry.path()) {
                Ok(it) => it,
                Err(_) => continue,
            };
            for file in inner.flatten() {
                if let Some(name) = file.file_name().to_str() {
                    if is_valid_hash(name) {
                        out.push(name.to_string());
                    }
                }
            }
        }
        Ok(out)
    }

    /// Total bytes occupied, for the Settings → Advanced display.
    pub fn total_bytes(&self) -> Result<u64> {
        let mut total = 0u64;
        for hash in self.list()? {
            total += self.size_of(&hash).unwrap_or(0);
        }
        Ok(total)
    }

    /// Remove interrupted-write temporaries older than one hour.
    ///
    /// Run during maintenance. The age check means a concurrent write by
    /// another (hypothetical) instance is never clobbered.
    pub fn sweep_temporaries(&self) -> Result<usize> {
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let mut removed = 0usize;
        let outer = match fs::read_dir(&self.root) {
            Ok(it) => it,
            Err(_) => return Ok(0),
        };
        for entry in outer.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(inner) = fs::read_dir(entry.path()) else {
                continue;
            };
            for file in inner.flatten() {
                let name = file.file_name();
                let Some(name) = name.to_str() else { continue };
                if !name.starts_with('.') || !name.contains(".tmp.") {
                    continue;
                }
                let stale = file
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|m| m < cutoff)
                    .unwrap_or(false);
                if stale && fs::remove_file(file.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(tmp.path().join("blobs")).unwrap();
        (tmp, store)
    }

    #[test]
    fn hash_is_stable_and_well_known() {
        // Known SHA-256 of "abc".
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn length_prefixing_prevents_part_ambiguity() {
        assert_ne!(hash_parts(&[b"ab", b"c"]), hash_parts(&[b"a", b"bc"]));
    }

    #[test]
    fn put_get_round_trip_and_is_private() {
        let (_tmp, store) = store();
        let payload = b"\x89PNG\r\n\x1a\n binary \x00 data".to_vec();
        let hash = store.put(&payload).unwrap();

        assert_eq!(store.get(&hash).unwrap(), payload);
        assert!(store.exists(&hash));
        assert_eq!(store.size_of(&hash), Some(payload.len() as u64));

        let path = store.path_for(&hash).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "blobs must not be readable by other users");
    }

    #[test]
    fn put_is_idempotent_and_deduplicates() {
        let (_tmp, store) = store();
        let a = store.put(b"same bytes").unwrap();
        let b = store.put(b"same bytes").unwrap();
        assert_eq!(a, b);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn empty_payload_is_storable() {
        let (_tmp, store) = store();
        let hash = store.put(b"").unwrap();
        assert_eq!(store.get(&hash).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn remove_is_idempotent_and_prunes_fanout_dir() {
        let (_tmp, store) = store();
        let hash = store.put(b"x").unwrap();
        let dir = store.path_for(&hash).unwrap().parent().unwrap().to_path_buf();
        store.remove(&hash).unwrap();
        store.remove(&hash).unwrap();
        assert!(!store.exists(&hash));
        assert!(!dir.exists(), "empty fan-out directory should be pruned");
    }

    #[test]
    fn malformed_hashes_cannot_escape_the_store() {
        let (_tmp, store) = store();
        for bad in [
            "../../../etc/passwd",
            "..",
            "",
            "ZZZZ",
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD", // uppercase
            "ba7816bf", // too short
        ] {
            assert!(store.path_for(bad).is_err(), "accepted {bad:?}");
            assert!(store.get(bad).is_err());
            assert!(store.remove(bad).is_err());
            assert!(!store.exists(bad));
        }
    }

    #[test]
    fn list_skips_temporaries_and_junk() {
        let (_tmp, store) = store();
        let hash = store.put(b"real").unwrap();
        let dir = store.path_for(&hash).unwrap().parent().unwrap().to_path_buf();
        fs::write(dir.join(".deadbeef.tmp.1"), b"junk").unwrap();
        fs::write(dir.join("not-a-hash"), b"junk").unwrap();

        assert_eq!(store.list().unwrap(), vec![hash]);
    }

    #[test]
    fn sweep_removes_only_stale_temporaries() {
        let (_tmp, store) = store();
        let hash = store.put(b"real").unwrap();
        let dir = store.path_for(&hash).unwrap().parent().unwrap().to_path_buf();
        let fresh = dir.join(".aaaa.tmp.99");
        fs::write(&fresh, b"junk").unwrap();

        // A just-created temporary is left alone.
        assert_eq!(store.sweep_temporaries().unwrap(), 0);
        assert!(fresh.exists());
        assert!(store.exists(&hash));
    }

    #[test]
    fn truncated_blob_is_rewritten() {
        let (_tmp, store) = store();
        let payload = b"the full payload";
        let hash = store.put(payload).unwrap();
        // Simulate a write that was interrupted after the rename.
        fs::write(store.path_for(&hash).unwrap(), b"trunc").unwrap();

        let again = store.put(payload).unwrap();
        assert_eq!(again, hash);
        assert_eq!(store.get(&hash).unwrap(), payload.to_vec());
    }

    #[test]
    fn total_bytes_sums_all_blobs() {
        let (_tmp, store) = store();
        store.put(&[0u8; 100]).unwrap();
        store.put(&[1u8; 250]).unwrap();
        assert_eq!(store.total_bytes().unwrap(), 350);
    }
}
