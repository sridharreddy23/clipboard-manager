//! Manual check for the "Clear all freezes the UI" bug.
//!
//! Not a unit test: it needs a display, a GTK main loop and a nested modal
//! loop, which `cargo test` cannot provide meaningfully. Run it under Xvfb.
//!
//! The bug: opening a modal dialog took focus from the panel, the panel's
//! focus-out handler hid the panel, and that withdrew the dialog's transient
//! parent so the compositor never presented the dialog -- while `dialog.run()`
//! sat in a nested main loop. The UI appeared frozen.
//!
//! What this checks: with the panel showing, trigger the Clear all flow, then
//! from inside the nested loop report whether the *panel* is still mapped.
//! Panel visible => the fix holds. Panel hidden => the bug is back.
//!
//!   Xvfb :88 -screen 0 1280x900x24 &
//!   DISPLAY=:88 cargo run --example dialog_freeze_check

use std::cell::RefCell;
use std::rc::Rc;

use clipboard_manager::clipboard::shared_settings;
use clipboard_manager::config::Settings;
use clipboard_manager::model::{RawClip, Selection};
use clipboard_manager::storage::{BlobStore, Store};
use clipboard_manager::ui::popup::{self, Popup};
use gtk::prelude::*;

fn main() {
    gtk::init().expect("needs a display; run under Xvfb");
    clipboard_manager::ui::theme::install();

    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
    let mut store = Store::open_at(dir.path()).unwrap();

    let settings = Settings::default();
    for body in ["first entry", "second entry", "third entry"] {
        let raw = RawClip {
            offered: vec!["UTF8_STRING".to_string()],
            mime: "UTF8_STRING".to_string(),
            bytes: body.as_bytes().to_vec(),
            selection: Selection::Clipboard,
            source_app: None,
        };
        let clip = clipboard_manager::clipboard::normalize::normalize(&raw, &settings, &blobs)
            .unwrap()
            .unwrap();
        store.insert(&clip, true).unwrap();
    }

    let popup = Popup::new(popup::Deps {
        store: Rc::new(RefCell::new(store)),
        settings: shared_settings(settings),
        suppress: Rc::new(|_| {}),
        caveat: None,
        session: "check".into(),
        backend: "none".into(),
    });
    popup.show();

    // Report from *inside* the nested modal loop the confirmation dialog runs.
    {
        let popup = Rc::clone(&popup);
        glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
            let visible = popup.window().is_visible();
            println!(
                "PANEL_VISIBLE_DURING_DIALOG={visible}  -> {}",
                if visible { "PASS: the panel stayed up" } else { "FAIL: the panel hid itself" }
            );
            gtk::main_quit();
        });
    }

    {
        let popup = Rc::clone(&popup);
        glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
            println!("triggering Clear all...");
            popup.request_clear_all();
        });
    }

    gtk::main();
    println!("main loop exited cleanly");
}
