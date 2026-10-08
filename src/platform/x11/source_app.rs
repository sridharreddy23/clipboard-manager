//! Working out which application owns a selection (§16, §30).
//!
//! This is a best-effort heuristic, and it is worth being precise about why.
//!
//! X11 identifies a selection owner by window, and that window is usually not
//! the application's visible window. Both Qt and GTK own selections with a
//! dedicated, unmapped, 1×1 window that carries no `WM_CLASS` and no
//! `_NET_WM_PID` — exactly like the one this backend creates for receiving. So
//! three things are tried, in descending order of trustworthiness:
//!
//! 1. The owner window and its ancestors, looking for `_NET_WM_PID` and
//!    `WM_CLASS`. When present this is authoritative: it really is the client
//!    that owns the selection.
//! 2. `_NET_ACTIVE_WINDOW` on the root, same properties. This is a guess — the
//!    focused window is *usually* the one that was just copied from, but a
//!    background process can set the clipboard at any time — so it is only
//!    used when step 1 found nothing.
//! 3. Nothing. The exclusion list then cannot be applied to this clipping,
//!    which is why [`crate::privacy`] also recognises the
//!    `x-kde-passwordManagerHint` target that password managers set. That
//!    mechanism is exact and does not depend on any of this.
//!
//! A PID is only resolved through `/proc` after checking `WM_CLIENT_MACHINE`
//! against the local hostname. Without that check, a window forwarded over SSH
//! would contribute a PID from the *remote* machine, which would then be looked
//! up locally and attribute the clipping to whatever unrelated local process
//! happens to hold that number.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::NONE;

use super::Atoms;
use crate::model::AppIdentity;

/// How far up the window tree to look. Toolkit hierarchies are shallow; this is
/// a loop bound, not a tuning parameter.
const MAX_ANCESTORS: usize = 8;

/// Longest `WM_CLASS` / `WM_CLIENT_MACHINE` value we will read. Generous for
/// real values, small enough that a hostile peer cannot make us allocate.
const MAX_STRING_BYTES: u32 = 1024;

/// Identify the application owning `owner`, if possible.
///
/// Never fails: every failure mode (a window that has since been destroyed, an
/// unreadable property, a remote client) collapses to `None`, which the privacy
/// layer treats as "unknown application".
pub fn identify<C: Connection>(
    conn: &C,
    atoms: &Atoms,
    owner: Window,
    root: Window,
) -> Option<AppIdentity> {
    if let Some(identity) = from_window_tree(conn, atoms, owner, root) {
        return Some(identity);
    }

    let active = active_window(conn, atoms, root)?;
    if active == owner {
        // Already tried, and it told us nothing.
        return None;
    }
    from_window_tree(conn, atoms, active, root)
}

/// Walk from `window` towards the root, stopping at the first window that
/// identifies itself.
fn from_window_tree<C: Connection>(
    conn: &C,
    atoms: &Atoms,
    window: Window,
    root: Window,
) -> Option<AppIdentity> {
    let mut current = window;

    for _ in 0..MAX_ANCESTORS {
        if current == NONE || current == root {
            return None;
        }

        let identity = from_window(conn, atoms, current);
        if let Some(identity) = identity {
            return Some(identity);
        }

        // `query_tree` fails if the window was destroyed between the selection
        // change and now, which is common and not an error.
        let parent = conn.query_tree(current).ok()?.reply().ok()?.parent;
        current = parent;
    }
    None
}

/// Read the identifying properties of exactly one window.
fn from_window<C: Connection>(conn: &C, atoms: &Atoms, window: Window) -> Option<AppIdentity> {
    let wm_class = read_wm_class(conn, atoms, window);
    let pid = read_pid(conn, atoms, window);

    let (name, exe) = match pid {
        Some(pid) if is_local(conn, atoms, window) => read_proc(pid),
        _ => (None, None),
    };

    let identity = AppIdentity { name, exe, wm_class };
    if identity.is_empty() {
        None
    } else {
        Some(identity)
    }
}

/// `WM_CLASS` holds two NUL-separated strings: the instance name
/// (`"navigator"`) and the class (`"Firefox"`). The class is the stable one, so
/// it is preferred, with the instance name as a fallback.
fn read_wm_class<C: Connection>(conn: &C, atoms: &Atoms, window: Window) -> Option<String> {
    let reply = conn
        .get_property(
            false,
            window,
            atoms.WM_CLASS,
            AtomEnum::STRING,
            0,
            MAX_STRING_BYTES / 4,
        )
        .ok()?
        .reply()
        .ok()?;

    if reply.type_ == NONE || reply.value.is_empty() {
        return None;
    }

    let mut parts = reply
        .value
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned());

    let instance = parts.next();
    parts.next().or(instance)
}

fn read_pid<C: Connection>(conn: &C, atoms: &Atoms, window: Window) -> Option<u32> {
    let reply = conn
        .get_property(false, window, atoms._NET_WM_PID, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ == NONE {
        return None;
    }
    let pid = reply.value32()?.next();
    pid.filter(|pid| *pid > 0)
}

/// True when the window's client is running on this machine.
///
/// A missing `WM_CLIENT_MACHINE` is treated as local, which matches how window
/// managers interpret it: local clients commonly omit it.
fn is_local<C: Connection>(conn: &C, atoms: &Atoms, window: Window) -> bool {
    let Ok(cookie) = conn.get_property(
        false,
        window,
        atoms.WM_CLIENT_MACHINE,
        AtomEnum::STRING,
        0,
        MAX_STRING_BYTES / 4,
    ) else {
        return false;
    };
    let Ok(reply) = cookie.reply() else {
        return false;
    };
    if reply.type_ == NONE || reply.value.is_empty() {
        return true;
    }

    let claimed = String::from_utf8_lossy(&reply.value);
    let claimed = claimed.trim_end_matches('\0').trim();
    match hostname() {
        Some(host) => {
            // Compare only up to the first dot, so `host` and
            // `host.example.com` are recognised as the same machine.
            let short = |s: &str| s.split('.').next().unwrap_or(s).to_lowercase();
            short(claimed) == short(&host)
        }
        // If we cannot tell, assume remote and skip the /proc lookup. The
        // consequence is a missing process name, which is far better than a
        // wrong one feeding into the exclusion list.
        None => false,
    }
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Read the process name and executable path for a local PID.
///
/// Both reads are allowed to fail independently: `/proc/<pid>/exe` is a symlink
/// readable only by the owning user, and although the daemon runs as that user
/// (§28), a setuid process would still refuse.
fn read_proc(pid: u32) -> (Option<String>, Option<String>) {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty());

    (name, exe)
}

fn active_window<C: Connection>(conn: &C, atoms: &Atoms, root: Window) -> Option<Window> {
    let reply = conn
        .get_property(false, root, atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ == NONE {
        return None;
    }
    let window = reply.value32()?.next();
    window.filter(|window| *window != NONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_process_is_readable_through_proc() {
        let (name, exe) = read_proc(std::process::id());
        assert!(name.is_some(), "expected a process name for ourselves");
        assert!(exe.is_some(), "expected an executable path for ourselves");
    }

    #[test]
    fn an_impossible_pid_yields_nothing_rather_than_panicking() {
        // PIDs are bounded well below this, so the paths cannot exist.
        let (name, exe) = read_proc(u32::MAX);
        assert_eq!(name, None);
        assert_eq!(exe, None);
    }

    #[test]
    fn the_local_hostname_is_readable() {
        // If this ever fails on a supported system, `is_local` degrades to
        // "assume remote", which loses source-app detection entirely — so it is
        // worth knowing.
        assert!(hostname().is_some());
    }
}
