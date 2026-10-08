//! XDG-compliant path resolution and private-by-default file helpers.
//!
//! Every file this application creates may contain clipboard history, so all of
//! the helpers here create files `0600` and directories `0700` (requirements
//! §18 / §37). The process umask is additionally tightened at startup, so that
//! even a crash part-way through a write cannot leave a world-readable file
//! behind.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Directory mode for anything that may contain clipboard data.
pub const DIR_MODE: u32 = 0o700;
/// File mode for anything that may contain clipboard data.
pub const FILE_MODE: u32 = 0o600;

/// Restrict the process umask so that files created by us (and by SQLite, which
/// creates `-wal` and `-shm` side files on its own) are never group/other
/// readable.
///
/// Called once, before any storage is touched.
pub fn harden_umask() {
    // SAFETY: `umask` is always safe to call; it only mutates process state and
    // cannot fail. We are single-threaded at the point this runs.
    unsafe {
        libc::umask(0o077);
    }
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn home() -> PathBuf {
    env_dir("HOME").unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `$XDG_CONFIG_HOME`, falling back to `~/.config`.
pub fn xdg_config_home() -> PathBuf {
    env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"))
}

/// `$XDG_DATA_HOME`, falling back to `~/.local/share`.
pub fn xdg_data_home() -> PathBuf {
    env_dir("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local/share"))
}

/// `$XDG_STATE_HOME`, falling back to `~/.local/state`.
pub fn xdg_state_home() -> PathBuf {
    env_dir("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local/state"))
}

/// `$XDG_RUNTIME_DIR` if usable, else a private directory under the state home.
///
/// `XDG_RUNTIME_DIR` is the correct location for the IPC socket because it is
/// already per-user and `0700` and is cleaned on logout. It is absent in some
/// minimal/`su`-style environments, hence the fallback.
pub fn runtime_dir() -> PathBuf {
    env_dir("XDG_RUNTIME_DIR").unwrap_or_else(|| xdg_state_home().join("clipboard-manager/run"))
}

/// `$XDG_CONFIG_HOME/clipboard-manager`
pub fn config_dir() -> PathBuf {
    xdg_config_home().join("clipboard-manager")
}

/// The configuration file.
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// `$XDG_DATA_HOME/clipboard-manager`, unless overridden in settings.
pub fn default_data_dir() -> PathBuf {
    xdg_data_home().join("clipboard-manager")
}

/// Per-session IPC directory. The session id keeps two concurrent graphical
/// sessions for the same user (e.g. a Wayland and an X11 login) from fighting
/// over one socket.
pub fn ipc_dir() -> PathBuf {
    runtime_dir().join("clipboard-manager")
}

/// A short, filesystem-safe identifier for the current graphical session.
pub fn session_id() -> String {
    let raw = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("DISPLAY").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "headless".to_string());

    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

/// Unix socket used by the CLI to talk to the daemon.
pub fn ipc_socket() -> PathBuf {
    ipc_dir().join(format!("{}.sock", session_id()))
}

/// Lock file backing single-instance enforcement.
pub fn ipc_lock() -> PathBuf {
    ipc_dir().join(format!("{}.lock", session_id()))
}

/// Create `path` (and parents) and force it to [`DIR_MODE`].
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create directory {}", path.display()))?;
    // Re-apply the mode unconditionally: `create_dir_all` leaves pre-existing
    // directories alone, and a directory created by an older version (or by a
    // careless migration) may be too permissive.
    let mut perms = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions();
    if perms.mode() & 0o777 != DIR_MODE {
        perms.set_mode(DIR_MODE);
        fs::set_permissions(path, perms)
            .with_context(|| format!("failed to secure directory {}", path.display()))?;
    }
    Ok(())
}

/// Force an existing file to [`FILE_MODE`]. Missing files are not an error.
pub fn secure_file(path: &Path) -> Result<()> {
    match fs::metadata(path) {
        Ok(meta) => {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != FILE_MODE {
                perms.set_mode(FILE_MODE);
                fs::set_permissions(path, perms)
                    .with_context(|| format!("failed to secure {}", path.display()))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to stat {}", path.display())),
    }
}

/// Write `bytes` to `path` atomically, with mode [`FILE_MODE`].
///
/// The temporary file is created in the same directory so that the final
/// `rename` is atomic, and is removed if anything fails.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    ensure_private_dir(parent)?;

    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("clipboard-manager"),
        std::process::id()
    ));

    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&tmp)
            .with_context(|| format!("failed to open {}", tmp.display()))?;
        file.write_all(bytes)?;
        // fsync before rename: on a power loss we would otherwise be able to
        // observe the rename without the data.
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_filesystem_safe() {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::set_var("DISPLAY", ":0.0");
        let id = session_id();
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(id, "_0_0");
    }

    #[test]
    fn private_dir_is_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b/c");
        ensure_private_dir(&nested).unwrap();
        let mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn atomic_write_is_0600_and_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("sub/file.bin");
        write_private_atomic(&target, b"first").unwrap();
        write_private_atomic(&target, b"second").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"second");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // No temporary files left behind.
        let leftovers: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty());
    }
}
