//! Global shortcut by `XGrabKey` on the root window (§3, §4).
//!
//! On an X11 session this is the whole mechanism: ask the server to deliver
//! `KeyPress` for one key combination no matter which window has focus. It
//! needs no desktop cooperation, which makes it the most reliable option we
//! have — and the only one that works identically on GNOME, KDE, XFCE,
//! Cinnamon and MATE.
//!
//! # Lock modifiers
//!
//! A grab matches the modifier mask *exactly*. `Ctrl+grave` grabbed with just
//! `ControlMask` therefore stops working the moment Num Lock is on, because the
//! event then carries `ControlMask | Mod2Mask`. This is the single most common
//! bug in hand-rolled X11 hotkey code, and the fix is to register the grab once
//! for every combination of the "don't care" lock modifiers — Caps Lock, Num
//! Lock and Scroll Lock — which is what [`lock_combinations`] enumerates.
//!
//! # Keyboard layouts
//!
//! The key is resolved by *keysym name*, not by character: we ask the server
//! which keycodes currently produce the `grave` keysym and grab all of them.
//! On a layout where no key produces it, registration fails and says so rather
//! than grabbing an arbitrary key (§3).
//!
//! # Not usable under Wayland
//!
//! A grab on the XWayland root only sees events while an X11 client has focus,
//! so it would appear to work and then not fire for native Wayland
//! applications. [`crate::shortcuts`] therefore never selects this backend for
//! a Wayland session, and that is a correctness requirement, not a preference.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeKeyboardControlAux, ConnectionExt as _, GrabMode, Keycode, ModMask,
};
use x11rb::protocol::Event;

use crate::platform::wake::{self, Ready, WakePipe};
use crate::shortcuts::keys::Accelerator;

/// The lock modifiers whose state must not affect whether the shortcut fires.
const LOCK_MODIFIERS: [ModMask; 3] = [
    ModMask::LOCK, // Caps Lock
    ModMask::M2,   // Num Lock, by near-universal convention
    ModMask::M5,   // Scroll Lock on many layouts
];

/// A live grab. Dropping it releases the key back to the rest of the desktop.
pub struct Grab {
    stop: Arc<AtomicBool>,
    wake: Arc<WakePipe>,
    thread: Option<std::thread::JoinHandle<()>>,
    detail: String,
}

impl Grab {
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.wake();
        if let Some(thread) = self.thread.take() {
            // Joining matters: the grab is only released when the connection
            // closes, and leaving it dangling would keep the key captured.
            let _ = thread.join();
        }
    }
}

/// Grab `accel` globally and call `on_press` on the GTK main thread.
///
/// The callback is invoked through `glib::idle_add_once`, so it runs on the main
/// loop and may touch GTK freely even though the grab lives on its own thread.
pub fn grab<F>(accel: &Accelerator, on_press: F) -> Result<Grab>
where
    F: Fn() + Send + Sync + 'static,
{
    let (conn, screen_num) = x11rb::connect(None).context("could not connect to the X server")?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .context("the X server reported no screens")?
        .root;

    let keycodes = keycodes_for(&conn, &accel.key)?;
    let base_mask = mod_mask(accel);

    // Register every lock-modifier variant, so Num Lock and Caps Lock cannot
    // break the shortcut.
    let combinations = lock_combinations();
    let mut grabbed = 0usize;
    let mut first_error = None;

    for keycode in &keycodes {
        for extra in &combinations {
            let mask = base_mask | *extra;
            // Checked, because a conflicting grab from another application is
            // reported asynchronously as a BadAccess error otherwise, and we
            // need to know now whether we actually hold the key.
            match conn
                .grab_key(
                    true, // report events relative to the grab window
                    root,
                    mask,
                    *keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .and_then(|cookie| Ok(cookie.check()))
            {
                Ok(Ok(())) => grabbed += 1,
                Ok(Err(err)) => {
                    first_error.get_or_insert(format!("{err}"));
                }
                Err(err) => {
                    first_error.get_or_insert(format!("{err}"));
                }
            }
        }
    }

    if grabbed == 0 {
        // §3: do not claim the shortcut works when it does not. The most
        // likely cause is another application already holding the key.
        let reason = first_error.unwrap_or_else(|| "the X server refused the grab".to_string());
        bail!(
            "could not grab {}: {reason}. Another application is probably already \
             using this shortcut.",
            accel.to_display()
        );
    }

    conn.flush()?;

    let stop = Arc::new(AtomicBool::new(false));
    let wake = WakePipe::new()?;
    let detail = format!(
        "grabbed directly from the X server ({} keycode{}, {} modifier variants)",
        keycodes.len(),
        if keycodes.len() == 1 { "" } else { "s" },
        combinations.len()
    );

    let thread_stop = Arc::clone(&stop);
    let thread_wake = Arc::clone(&wake);
    let thread = std::thread::Builder::new()
        .name("clip-hotkey".to_string())
        .spawn(move || {
            run(conn, thread_stop, thread_wake, on_press);
        })
        .context("could not start the shortcut listener thread")?;

    Ok(Grab { stop, wake, thread: Some(thread), detail })
}

/// Event loop for the grab. Blocks in `poll(2)`; never polls the keyboard.
fn run<F>(
    conn: x11rb::rust_connection::RustConnection,
    stop: Arc<AtomicBool>,
    wake: Arc<WakePipe>,
    on_press: F,
) where
    F: Fn() + Send + Sync + 'static,
{
    // A callback shared with the main loop has to be reference-counted and
    // `Send`; `Arc` rather than `Rc` because it crosses the thread boundary.
    let on_press = Arc::new(on_press);

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }

        // Drain everything already parsed before sleeping on the socket, or an
        // event buffered during a reply would leave us waiting on a quiet fd.
        loop {
            match conn.poll_for_event() {
                Ok(Some(Event::KeyPress(_))) => {
                    let callback = Arc::clone(&on_press);
                    // Hop to the GTK main thread: the callback shows a window.
                    glib::idle_add_once(move || callback());
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(err) => {
                    // A broken connection means the X server went away, which
                    // happens on logout. Exiting quietly is correct; the daemon
                    // is going down with it.
                    tracing::debug!(error = %err, "shortcut listener connection closed");
                    return;
                }
            }
        }

        if let Err(err) = conn.flush() {
            tracing::debug!(error = %err, "shortcut listener could not flush");
            return;
        }

        use std::os::fd::AsFd;
        match wake::wait(conn.stream().as_fd(), wake.read_fd(), None) {
            Ok(Ready::Socket) => {}
            Ok(Ready::Woken) => {
                wake.drain();
                if stop.load(Ordering::Relaxed) {
                    return;
                }
            }
            Ok(Ready::TimedOut) => {}
            Err(err) => {
                tracing::debug!(error = %err, "shortcut listener wait failed");
                return;
            }
        }
    }
}

/// Our modifier representation as an X11 mask.
fn mod_mask(accel: &Accelerator) -> ModMask {
    let mut mask = ModMask::from(0u16);
    if accel.mods.ctrl {
        mask = mask | ModMask::CONTROL;
    }
    if accel.mods.shift {
        mask = mask | ModMask::SHIFT;
    }
    if accel.mods.alt {
        mask = mask | ModMask::M1;
    }
    if accel.mods.superkey {
        mask = mask | ModMask::M4;
    }
    mask
}

/// Every subset of the lock modifiers, as masks to OR onto the real one.
///
/// Eight entries for three independent locks. Registering all of them is what
/// makes the shortcut work with Num Lock on.
fn lock_combinations() -> Vec<ModMask> {
    let mut out = Vec::with_capacity(1 << LOCK_MODIFIERS.len());
    for bits in 0..(1u8 << LOCK_MODIFIERS.len()) {
        let mut mask = ModMask::from(0u16);
        for (index, modifier) in LOCK_MODIFIERS.iter().enumerate() {
            if bits & (1 << index) != 0 {
                mask = mask | *modifier;
            }
        }
        out.push(mask);
    }
    out
}

/// Which keycodes currently produce the named keysym.
///
/// Plural because a keysym can sit on several physical keys — `grave` is on the
/// main key and, on some layouts, a keypad or secondary position — and a user
/// pressing either should open the panel.
fn keycodes_for(conn: &impl Connection, key_name: &str) -> Result<Vec<Keycode>> {
    let keysym = keysym_for(key_name)
        .with_context(|| format!("{key_name:?} is not a key this system knows"))?;

    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;

    let mapping = conn
        .get_keyboard_mapping(min, max - min + 1)
        .context("could not read the keyboard mapping")?
        .reply()
        .context("could not read the keyboard mapping")?;

    let per_code = mapping.keysyms_per_keycode as usize;
    if per_code == 0 {
        bail!("the X server reported an empty keyboard mapping");
    }

    let mut found = Vec::new();
    for (index, chunk) in mapping.keysyms.chunks(per_code).enumerate() {
        // Only the unshifted and shifted levels: binding to a third-level
        // keysym would need the layout's level-3 modifier held too, which is
        // not what the user asked for.
        if chunk.iter().take(2).any(|candidate| *candidate == keysym) {
            let keycode = min as usize + index;
            if keycode <= max as usize {
                found.push(keycode as Keycode);
            }
        }
    }

    if found.is_empty() {
        bail!(
            "no key on the current keyboard layout produces {key_name:?}; \
             choose a different shortcut"
        );
    }
    Ok(found)
}

/// Map a keysym *name* to its numeric value.
///
/// Only the names [`crate::shortcuts::keys`] can produce need to be here. The
/// values are from `keysymdef.h` and are stable ABI — they have not changed
/// since X11R4 and cannot, since they are baked into every X client ever built.
fn keysym_for(name: &str) -> Option<u32> {
    // Letters and digits are Latin-1, where the keysym equals the ASCII code.
    if name.chars().count() == 1 {
        let ch = name.chars().next()?;
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            return Some(ch as u32);
        }
    }

    // Function keys are consecutive from F1 = 0xffbe.
    if let Some(digits) = name.strip_prefix('F') {
        if let Ok(number) = digits.parse::<u32>() {
            if (1..=24).contains(&number) {
                return Some(0xffbe + number - 1);
            }
        }
    }

    Some(match name {
        "grave" => 0x0060,
        "asciitilde" => 0x007e,
        "minus" => 0x002d,
        "underscore" => 0x005f,
        "equal" => 0x003d,
        "plus" => 0x002b,
        "bracketleft" => 0x005b,
        "bracketright" => 0x005d,
        "braceleft" => 0x007b,
        "braceright" => 0x007d,
        "backslash" => 0x005c,
        "bar" => 0x007c,
        "semicolon" => 0x003b,
        "colon" => 0x003a,
        "apostrophe" => 0x0027,
        "quotedbl" => 0x0022,
        "comma" => 0x002c,
        "period" => 0x002e,
        "slash" => 0x002f,
        "question" => 0x003f,
        "space" => 0x0020,
        "Tab" => 0xff09,
        "Return" => 0xff0d,
        "Escape" => 0xff1b,
        "BackSpace" => 0xff08,
        "Delete" => 0xffff,
        "Insert" => 0xff63,
        "Home" => 0xff50,
        "End" => 0xff57,
        "Prior" => 0xff55,
        "Next" => 0xff56,
        "Up" => 0xff52,
        "Down" => 0xff54,
        "Left" => 0xff51,
        "Right" => 0xff53,
        "Print" => 0xff61,
        "Menu" => 0xff67,
        "Pause" => 0xff13,
        _ => return None,
    })
}

/// Silence the "bell" some servers ring on a refused grab. Unused but kept
/// adjacent to the grab code it belongs with.
#[allow(dead_code)]
fn quiet_bell(conn: &impl Connection) {
    let _ = conn.change_keyboard_control(&ChangeKeyboardControlAux::new().bell_percent(0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_lock_modifier_combination_is_registered() {
        // Eight subsets of three locks. Missing any of them is the classic
        // "my shortcut stops working with Num Lock on" bug.
        let combinations = lock_combinations();
        assert_eq!(combinations.len(), 8);

        // The empty mask must be present, or the shortcut would *only* work
        // with a lock engaged.
        assert!(combinations.iter().any(|m| u16::from(*m) == 0));
        // And the all-locks mask, for the user with every light on.
        let all = u16::from(ModMask::LOCK) | u16::from(ModMask::M2) | u16::from(ModMask::M5);
        assert!(combinations.iter().any(|m| u16::from(*m) == all));

        // No duplicates: a repeated grab would return BadAccess against
        // ourselves and look like a conflict.
        let mut seen: Vec<u16> = combinations.iter().map(|m| u16::from(*m)).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before);
    }

    #[test]
    fn the_default_shortcut_maps_to_control_plus_the_grave_keysym() {
        let accel = Accelerator::parse(crate::config::DEFAULT_SHORTCUT).unwrap();
        assert_eq!(u16::from(mod_mask(&accel)), u16::from(ModMask::CONTROL));
        // 0x60 is `grave` in keysymdef.h, and equals ASCII '`'.
        assert_eq!(keysym_for(&accel.key), Some(0x0060));
        assert_eq!(keysym_for("grave"), Some('`' as u32));
    }

    #[test]
    fn modifiers_combine_rather_than_overwrite() {
        let accel = Accelerator::parse("Ctrl+Shift+Alt+Super+v").unwrap();
        let mask = u16::from(mod_mask(&accel));
        for expected in [ModMask::CONTROL, ModMask::SHIFT, ModMask::M1, ModMask::M4] {
            assert_ne!(mask & u16::from(expected), 0, "{expected:?} missing from {mask:#x}");
        }
    }

    #[test]
    fn letters_digits_and_function_keys_resolve_to_keysyms() {
        assert_eq!(keysym_for("v"), Some('v' as u32));
        assert_eq!(keysym_for("7"), Some('7' as u32));
        assert_eq!(keysym_for("F1"), Some(0xffbe));
        assert_eq!(keysym_for("F12"), Some(0xffc9));
        assert_eq!(keysym_for("Escape"), Some(0xff1b));
    }

    #[test]
    fn an_unknown_key_name_is_rejected_rather_than_mapped_to_zero() {
        // Mapping to 0 would grab keycode 0 and silently never fire.
        assert_eq!(keysym_for("NotAKey"), None);
        assert_eq!(keysym_for("F99"), None);
    }
}
