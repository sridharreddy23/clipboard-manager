//! The clipboard history panel (§5, §6, §7, §32, §40).
//!
//! A compact floating panel that appears near the pointer, stays above other
//! windows, and closes when it loses focus, when Escape is pressed, or when an
//! entry is chosen. Layout, top to bottom: title, optional platform-limitation
//! banner, search field, scrolling list, and "Clear all".
//!
//! # Re-entrancy
//!
//! The [`Store`] lives behind a `RefCell` and the whole UI is single-threaded,
//! so there is no lock — but GTK's modal dialogs run a *nested* main loop, and
//! anything on the stack when one opens is still on the stack when a signal
//! fires inside it. Holding a `RefCell` borrow across `dialog.run()` would
//! therefore panic the moment the user clicked a row behind the dialog.
//!
//! Every handler here follows the same shape, and it is load-bearing rather
//! than stylistic:
//!
//! 1. borrow the store, copy out what is needed, **drop the borrow**;
//! 2. run the dialog;
//! 3. borrow again to write.
//!
//! # Paste behaviour
//!
//! Activating an entry restores it to the clipboard and closes the panel. It
//! does not synthesise a keystroke into the previously focused application —
//! §51 forbids that, and [`crate::ui::selection`] explains why. The user then
//! presses their own paste key, exactly as with the Windows flyout.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;

use crate::clipboard::SharedSettings;
use crate::config::ActivateAction;
use crate::model::{self, ClipItem, ClipKind};
use crate::storage::Store;
use crate::ui::row::{self, RowAction};
use crate::ui::{dialogs, selection, theme};

/// How many entries the list renders at once.
///
/// The store can hold a hundred thousand; building a hundred thousand GTK
/// widgets would take seconds and megabytes. Search is how you reach the rest,
/// which is the same bargain the Windows flyout makes (§46).
const PAGE: usize = 200;

// Rows are ~56px and the tallest configurable panel is 1000px, so ~18 rows are
// visible. A page well above that keeps scrolling smooth without building
// widgets for a 100k history (§46). Compile-time, so it cannot be folded away.
const _: () = assert!(PAGE >= 100);

/// Thumbnails cached in memory, keyed by blob hash.
///
/// Bounded because the cache lives as long as the daemon: an unbounded one
/// would be a slow leak for a user who copies a lot of screenshots.
const THUMB_CACHE_LIMIT: usize = 96;

// Unbounded would be a slow leak for a heavy screenshot user; unreasonably
// large would defeat the point of bounding it at all.
const _: () = assert!(THUMB_CACHE_LIMIT > 0 && THUMB_CACHE_LIMIT <= 256);

/// Gap between the pointer and the panel's edge.
const CURSOR_MARGIN: i32 = 12;

/// How long after opening the panel ignores a focus-out.
///
/// The panel dismisses itself when it loses focus, which is how click-outside
/// works. But a window does not reliably *hold* focus for the first instant
/// after `present()`: under GNOME Wayland, focus-stealing prevention and the
/// ordinary map/focus handshake produce a transient focus-out, and the panel
/// would hide itself a few milliseconds after opening.
///
/// The user-visible symptom was "sometimes Ctrl+` doesn't work" — the panel
/// flashed and vanished, and because it had hidden itself the *next* press
/// opened it, so it felt like every other press worked. Measured on a live
/// GNOME Wayland session: three of six presses self-dismissed.
///
/// 400 ms is long enough to cover the handshake on a loaded machine and short
/// enough that a deliberate click away still dismisses the panel immediately
/// — nobody opens this and clicks elsewhere inside half a second.
const FOCUS_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// How long the panel must stay unfocused before it dismisses itself.
///
/// Focus-out alone is not a reliable dismissal signal here. Measured on a live
/// GNOME Wayland session, an open panel gains focus after ~30 ms and then
/// *flaps*: lost at 1.8 s, regained at 2.6 s, lost at 3.1 s, with no user
/// input at all. Hiding on the first focus-out therefore closed the panel by
/// itself a second or two after opening — which the user experiences as
/// "sometimes Ctrl+` doesn't work", because the panel flashed up and vanished,
/// and the next press then re-opened it.
///
/// So a focus-out only *arms* a timer. If focus comes back, the timer is
/// cancelled; when it fires, the real focus state is re-checked before
/// hiding. A genuine click elsewhere keeps focus elsewhere and the panel
/// closes after this delay, which is short enough to feel immediate.
const FOCUS_SETTLE: std::time::Duration = std::time::Duration::from_millis(350);

/// Stands in for a section heading in the parallel `shown` list.
///
/// SQLite rowids start at 1 and are always positive, so no real entry can
/// collide with this.
const SECTION_SENTINEL: i64 = i64::MIN;
const _: () = assert!(SECTION_SENTINEL < 1);

/// Things the popup needs from the daemon, passed in rather than reached for so
/// this type has no global state and can be constructed in a test.
pub struct Deps {
    pub store: Rc<RefCell<Store>>,
    pub settings: SharedSettings,
    /// Tells the monitor to ignore the next clipping with this hash, so
    /// restoring an entry does not come straight back as a new one.
    pub suppress: Rc<dyn Fn(&str)>,
    /// Platform limitation reported by the chosen backend, if any.
    ///
    /// Shown in the About dialog and by `clipboard-manager status`, not in the
    /// panel. §20 asks for platform limitations to be documented clearly, and
    /// they are — in About, in `status`, in the Privacy tab where it affects a
    /// setting, and in the README. A permanent banner in a compact panel the
    /// user opens dozens of times a day is nagging, not documenting: it costs
    /// a row of list space forever to tell someone something they learned the
    /// first time.
    pub caveat: Option<String>,
    pub session: String,
    pub backend: String,
}

pub struct Popup {
    window: gtk::Window,
    search: gtk::SearchEntry,
    list: gtk::ListBox,
    scroller: gtk::ScrolledWindow,
    empty: gtk::Box,
    empty_title: gtk::Label,
    empty_body: gtk::Label,
    clear_button: gtk::Button,
    deps: Deps,
    /// Row ids in display order, so keyboard navigation and the id of a
    /// selected row can be resolved without walking widgets.
    shown: RefCell<Vec<i64>>,
    thumbs: RefCell<HashMap<String, gdk_pixbuf::Pixbuf>>,
    /// Set while `refresh` is rebuilding, so the row-selection signal it
    /// triggers does not fire an activation.
    rebuilding: Cell<bool>,
    /// When the panel was last shown, for [`FOCUS_GRACE`].
    shown_at: Cell<Option<std::time::Instant>>,
    /// Generation counter for the pending focus-out timer.
    ///
    /// Bumped whenever focus returns or the panel is shown or hidden, so a
    /// timer armed by an earlier focus-out becomes a no-op instead of closing
    /// a panel the user is still using.
    focus_generation: Cell<u64>,
    /// How many modal dialogs this panel currently has open.
    ///
    /// A counter rather than a flag because dialogs nest: an error dialog can
    /// be raised from inside the edit flow.
    modal_depth: Cell<u32>,
    /// Opens the settings window.
    ///
    /// Settable after construction because the popup and the settings window
    /// are both owned by [`crate::daemon::Core`], and the popup has to exist
    /// first so the shortcut has something to toggle. A no-op default rather
    /// than an `Option` so the button is never wired to nothing.
    on_open_settings: RefCell<Rc<dyn Fn()>>,
}

impl Popup {
    pub fn new(deps: Deps) -> Rc<Self> {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Clipboard history");
        window.set_decorated(false);
        window.set_resizable(false);
        // Not a normal window: no taskbar button, no pager entry, above the
        // windows it floats over.
        window.set_skip_taskbar_hint(true);
        window.set_skip_pager_hint(true);
        window.set_keep_above(true);
        window.set_type_hint(gdk::WindowTypeHint::Dialog);
        window.set_accept_focus(true);
        window.set_focus_on_map(true);
        window.set_icon_name(Some("edit-paste"));

        enable_transparency(&window);

        // The shadow and rounded corners live on this inner box, because a GTK
        // toplevel cannot cast a shadow itself.
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.style_context().add_class("cm-frame");

        // ---- header ----
        let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
        header.style_context().add_class("cm-header");

        let title = gtk::Label::new(Some("Clipboard history"));
        title.style_context().add_class("cm-title");
        title.set_xalign(0.0);
        header.add(&title);

        let search = gtk::SearchEntry::new();
        search.style_context().add_class("cm-search");
        search.set_placeholder_text(Some("Search clipboard history"));
        // Screen readers announce this; the placeholder alone is not a label.
        search.set_tooltip_text(Some("Search clipboard history (Ctrl+F)"));
        header.add(&search);

        frame.add(&header);

        // ---- list ----
        let list = gtk::ListBox::new();
        list.style_context().add_class("cm-list");
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&list)
            .build();
        frame.add(&scroller);

        // ---- empty state ----
        let empty = gtk::Box::new(gtk::Orientation::Vertical, 6);
        empty.style_context().add_class("cm-empty");
        empty.set_valign(gtk::Align::Center);
        let empty_title = gtk::Label::new(None);
        empty_title.style_context().add_class("cm-empty-title");
        let empty_body = gtk::Label::new(None);
        empty_body.style_context().add_class("cm-empty-body");
        empty_body.set_line_wrap(true);
        empty_body.set_justify(gtk::Justification::Center);
        empty_body.set_max_width_chars(36);
        empty.add(&empty_title);
        empty.add(&empty_body);
        frame.add(&empty);

        // ---- footer ----
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        footer.style_context().add_class("cm-footer");

        let settings_button = gtk::Button::from_icon_name(
            Some("preferences-system-symbolic"),
            gtk::IconSize::Button,
        );
        settings_button.set_relief(gtk::ReliefStyle::None);
        settings_button.set_tooltip_text(Some("Settings"));
        footer.add(&settings_button);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        footer.add(&spacer);

        let clear_button = gtk::Button::with_label("Clear all");
        clear_button.set_relief(gtk::ReliefStyle::None);
        clear_button.set_tooltip_text(Some("Delete every unpinned entry"));
        footer.add(&clear_button);

        frame.add(&footer);
        window.add(&frame);

        let popup = Rc::new(Self {
            window,
            search,
            list,
            scroller,
            empty,
            empty_title,
            empty_body,
            clear_button,
            deps,
            shown: RefCell::new(Vec::new()),
            thumbs: RefCell::new(HashMap::new()),
            rebuilding: Cell::new(false),
            shown_at: Cell::new(None),
            focus_generation: Cell::new(0),
            modal_depth: Cell::new(0),
            on_open_settings: RefCell::new(Rc::new(|| {
                tracing::warn!("the settings button was pressed before it was wired up");
            })),
        });

        popup.wire(&settings_button);
        popup
    }

    fn wire(self: &Rc<Self>, settings_button: &gtk::Button) {
        // -- search --
        {
            let this = Rc::clone(self);
            self.search.connect_search_changed(move |_| this.refresh());
        }

        // -- activation --
        {
            let this = Rc::clone(self);
            self.list.connect_row_activated(move |_, row| {
                if this.rebuilding.get() {
                    return;
                }
                let index = row.index();
                if let Some(id) = this.shown.borrow().get(index as usize).copied() {
                    this.handle(RowAction::Activate(id));
                }
            });
        }

        // -- clear all --
        {
            let this = Rc::clone(self);
            self.clear_button.connect_clicked(move |_| this.clear_all());
        }

        // -- settings --
        {
            let this = Rc::clone(self);
            settings_button.connect_clicked(move |_| {
                // Hide first: the settings window is a normal window, and
                // leaving an always-on-top panel over it looks broken.
                this.hide();
                let open = Rc::clone(&this.on_open_settings.borrow());
                open();
            });
        }

        // -- keyboard --
        {
            let this = Rc::clone(self);
            self.window.connect_key_press_event(move |_, event| this.on_key(event));
        }

        // -- close on focus loss --
        //
        // This is how "click outside to dismiss" is implemented. A pointer grab
        // would be the other option, but grabs misbehave across compositors and
        // can leave the desktop unclickable if we crash while holding one;
        // losing focus is both safer and what the user already expects.
        {
            let this = Rc::clone(self);
            self.window.connect_focus_out_event(move |_, _| {
                if this.modal_depth.get() > 0 {
                    return glib::Propagation::Proceed;
                }
                // Ignore the wobble that follows `present()`; see `FOCUS_GRACE`.
                if let Some(shown) = this.shown_at.get() {
                    if shown.elapsed() < FOCUS_GRACE {
                        return glib::Propagation::Proceed;
                    }
                }
                if !this.settings().appearance.close_on_focus_loss {
                    return glib::Propagation::Proceed;
                }
                this.arm_focus_dismiss();
                glib::Propagation::Proceed
            });
        }

        {
            let this = Rc::clone(self);
            self.window.connect_focus_in_event(move |_, _| {
                // Focus came back, so any armed dismissal was a flap.
                this.focus_generation.set(this.focus_generation.get() + 1);
                glib::Propagation::Proceed
            });
        }

        // Closing the panel must never close the application: the daemon keeps
        // monitoring, and the panel is reopened by the shortcut.
        self.window.connect_delete_event(|window, _| {
            window.hide();
            glib::Propagation::Stop
        });
    }

    // ---- visibility ----------------------------------------------------------

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn toggle(self: &Rc<Self>) {
        tracing::debug!(visible = self.is_visible(), "toggle requested");
        if self.is_visible() {
            self.hide();
        } else {
            self.show();
        }
    }

    /// Arm the delayed focus dismissal. See [`FOCUS_SETTLE`].
    fn arm_focus_dismiss(self: &Rc<Self>) {
        let generation = self.focus_generation.get() + 1;
        self.focus_generation.set(generation);

        let this = Rc::clone(self);
        glib::timeout_add_local_once(FOCUS_SETTLE, move || {
            // Superseded by a focus-in, a show, or a hide.
            if this.focus_generation.get() != generation {
                return;
            }
            if this.modal_depth.get() > 0 || !this.window.is_visible() {
                return;
            }
            // Re-check reality rather than trusting the event that armed us.
            if this.window.has_toplevel_focus() {
                return;
            }
            tracing::debug!("panel stayed unfocused; hiding");
            this.hide();
        });
    }

    pub fn hide(&self) {
        // Cleared so a stale stamp cannot grant a grace period to the *next*
        // focus-out after the panel has been reopened by some other path, and
        // bumped so any armed dismissal is cancelled.
        self.shown_at.set(None);
        self.focus_generation.set(self.focus_generation.get() + 1);
        self.window.hide();
    }

    /// Open the panel near the pointer and focus the search field.
    pub fn show(self: &Rc<Self>) {
        let settings = self.settings();
        let resolved = theme::resolve(settings.appearance.theme);
        theme::apply(&self.window, resolved);

        self.window
            .set_size_request(settings.appearance.popup_width, settings.appearance.popup_height);
        self.window.resize(settings.appearance.popup_width, settings.appearance.popup_height);

        // Clear the previous query so the panel always opens on the full list.
        // Keeping the last search would be a small convenience and a large
        // surprise.
        self.search.set_text("");
        self.refresh();

        self.window.show_all();
        self.sync_empty_state();
        self.position_near_pointer();

        // Stamped before `present()`, so the grace period covers the whole
        // map/focus handshake rather than starting after it.
        self.shown_at.set(Some(std::time::Instant::now()));
        self.focus_generation.set(self.focus_generation.get() + 1);
        self.window.present();
        self.search.grab_focus();
        tracing::debug!(visible = self.window.is_visible(), "panel shown");
    }

    /// Place the panel beside the pointer, kept inside the monitor's work area.
    fn position_near_pointer(&self) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let Some(seat) = display.default_seat() else {
            return;
        };
        let Some(pointer) = seat.pointer() else {
            return;
        };

        let (screen, x, y) = pointer.position();
        let monitor = display
            .monitor_at_point(x, y)
            .or_else(|| display.primary_monitor())
            .or_else(|| display.monitor(0));
        let Some(monitor) = monitor else {
            return;
        };

        // Work area excludes panels and docks, so the popup never lands under
        // the GNOME top bar or a Plasma taskbar.
        let area = monitor.workarea();
        let (width, height) = self.window.size();

        // Prefer below-right of the pointer, flipping when that would overflow —
        // the same rule a context menu follows.
        let mut px = x + CURSOR_MARGIN;
        let mut py = y + CURSOR_MARGIN;
        if px + width > area.x() + area.width() {
            px = x - width - CURSOR_MARGIN;
        }
        if py + height > area.y() + area.height() {
            py = y - height - CURSOR_MARGIN;
        }

        // Clamp as a backstop, for a pointer in a corner or a panel larger than
        // the work area.
        let px = px.clamp(area.x(), (area.x() + area.width() - width).max(area.x()));
        let py = py.clamp(area.y(), (area.y() + area.height() - height).max(area.y()));

        self.window.set_screen(&screen);
        self.window.move_(px, py);
    }

    // ---- list ----------------------------------------------------------------

    fn settings(&self) -> crate::config::Settings {
        match self.deps.settings.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Rebuild the list from the store, honouring the current search text.
    pub fn refresh(self: &Rc<Self>) {
        let query = self.search.text().to_string();
        let settings = self.settings();

        // Borrow, query, drop. The borrow must not survive into row building,
        // which runs callbacks that may want the store again.
        let items: Vec<ClipItem> = {
            let store = self.deps.store.borrow();
            let result = if query.trim().is_empty() {
                store.recent(PAGE)
            } else {
                store.search(&query, PAGE)
            };
            match result {
                Ok(items) => items,
                Err(err) => {
                    // A failed query must not blank the UI silently.
                    tracing::error!(error = %err, "could not read the history");
                    Vec::new()
                }
            }
        };

        self.rebuilding.set(true);
        for child in self.list.children() {
            self.list.remove(&child);
        }

        let now = model::now_ms();
        let mut shown = Vec::with_capacity(items.len());
        let mut pinned_header_done = false;
        let mut unpinned_header_done = false;
        let any_pinned = items.iter().any(|i| i.is_pinned);

        for item in &items {
            // Section labels, but only when there is a pinned group to separate
            // — a header over a single ungrouped list is just noise (§11).
            if any_pinned {
                if item.is_pinned && !pinned_header_done {
                    self.add_section_label("Pinned");
                    shown.push(SECTION_SENTINEL);
                    pinned_header_done = true;
                } else if !item.is_pinned && !unpinned_header_done {
                    self.add_section_label("Recent");
                    shown.push(SECTION_SENTINEL);
                    unpinned_header_done = true;
                }
            }

            let thumb = self.thumbnail_for(item, settings.appearance.thumbnail_size);
            let this = Rc::clone(self);
            let built = row::build(
                item,
                now,
                settings.appearance.show_timestamps,
                thumb.as_ref(),
                settings.appearance.thumbnail_size,
                move |action| this.handle(action),
            );
            self.list.add(&built.widget);
            shown.push(built.id);
        }

        *self.shown.borrow_mut() = shown;
        self.list.show_all();
        self.rebuilding.set(false);

        self.sync_empty_state_for(items.len(), &query);
        self.select_first_row();
    }

    /// A non-selectable heading inside the list.
    fn add_section_label(&self, text: &str) {
        let label = gtk::Label::new(Some(text));
        label.style_context().add_class("cm-section-label");
        label.set_xalign(0.0);
        let row = gtk::ListBoxRow::new();
        row.add(&label);
        row.set_selectable(false);
        row.set_activatable(false);
        self.list.add(&row);
    }

    fn select_first_row(&self) {
        // Skip the section headings, which are not selectable.
        let shown = self.shown.borrow();
        if let Some(index) = shown.iter().position(|id| *id != SECTION_SENTINEL) {
            if let Some(row) = self.list.row_at_index(index as i32) {
                self.list.select_row(Some(&row));
            }
        }
    }

    fn sync_empty_state(&self) {
        let count = self.shown.borrow().iter().filter(|id| **id != SECTION_SENTINEL).count();
        let query = self.search.text().to_string();
        self.sync_empty_state_for(count, &query);
    }

    fn sync_empty_state_for(&self, count: usize, query: &str) {
        if count > 0 {
            self.empty.hide();
            self.scroller.show();
            return;
        }

        self.scroller.hide();
        if query.trim().is_empty() {
            self.empty_title.set_text("Nothing copied yet");
            self.empty_body.set_text(
                "Copy something and it will appear here. \
                 Pinned entries are kept until you unpin them.",
            );
        } else {
            self.empty_title.set_text("No matches");
            self.empty_body
                .set_text(&format!("Nothing in the history matches “{query}”."));
        }
        self.empty.show_all();
    }

    /// A cached, pre-scaled thumbnail for an image row.
    fn thumbnail_for(&self, item: &ClipItem, size: i32) -> Option<gdk_pixbuf::Pixbuf> {
        if item.kind != ClipKind::Image {
            return None;
        }
        let hash = item.blob_hash.as_ref()?;

        if let Some(cached) = self.thumbs.borrow().get(hash) {
            return Some(cached.clone());
        }

        let bytes = {
            let store = self.deps.store.borrow();
            // A missing blob is not an error worth a dialog: the row falls back
            // to the type icon and stays usable (§35).
            store.blobs().get(hash).ok()?
        };
        let thumb = dialogs::thumbnail_from_bytes(&bytes, size)?;

        let mut cache = self.thumbs.borrow_mut();
        if cache.len() >= THUMB_CACHE_LIMIT {
            // Plain eviction rather than LRU: the list is rebuilt from the top
            // on every refresh, so the entries that matter are re-inserted
            // immediately and tracking recency would buy nothing.
            cache.clear();
        }
        cache.insert(hash.clone(), thumb.clone());
        Some(thumb)
    }

    /// Forget cached thumbnails, after a delete or a settings change.
    pub fn invalidate_thumbnails(&self) {
        self.thumbs.borrow_mut().clear();
    }

    // ---- actions -------------------------------------------------------------

    fn handle(self: &Rc<Self>, action: RowAction) {
        match action {
            RowAction::Activate(id) => self.restore(id, true),
            RowAction::Copy(id) => self.restore(id, false),
            RowAction::TogglePin(id) => self.toggle_pin(id),
            RowAction::Edit(id) => self.edit(id),
            RowAction::View(id) => self.view(id),
            RowAction::Delete(id) => self.delete(id),
        }
    }

    /// Put an entry back on the clipboard (§51, §52).
    fn restore(self: &Rc<Self>, id: i64, close: bool) {
        let loaded = {
            let store = self.deps.store.borrow();
            store.payload_of(id)
        };

        let (item, payload) = match loaded {
            Ok(pair) => pair,
            Err(err) => {
                tracing::error!(id, error = %err, "could not load the entry to restore");
                let _modal = self.modal();
                dialogs::error(
                    Some(&self.window),
                    "That entry could not be read",
                    "Its stored data is missing or unreadable. The entry has been left in \
                     place; deleting and re-copying is the simplest fix.",
                );
                return;
            }
        };

        // Register the hash *before* taking the selection, or the monitor will
        // see our own write as a new clipping and bounce the entry to the top.
        (self.deps.suppress)(&item.content_hash);

        if let Err(err) = selection::restore(&item, payload) {
            tracing::error!(id, error = %err, "could not take the clipboard selection");
            let _modal = self.modal();
            dialogs::error(
                Some(&self.window),
                "The clipboard is not available",
                "Another application is holding it. Try again in a moment.",
            );
            return;
        }

        // Moves the entry to the top of the history and refreshes its age.
        if let Err(err) = self.deps.store.borrow().touch(id) {
            // Cosmetic only — the clipboard already has the content, which is
            // what the user asked for.
            tracing::warn!(id, error = %err, "could not update the entry's last-used time");
        }

        let stay_open = !close
            || self.settings().advanced.activate_action == ActivateAction::CopyAndStayOpen;
        if stay_open {
            self.refresh();
        } else {
            self.hide();
        }
    }

    fn toggle_pin(self: &Rc<Self>, id: i64) {
        let current = {
            let store = self.deps.store.borrow();
            store.get(id).ok().flatten().map(|i| i.is_pinned)
        };
        let Some(pinned) = current else {
            return;
        };

        if let Err(err) = self.deps.store.borrow().set_pinned(id, !pinned) {
            tracing::error!(id, error = %err, "could not change the pinned state");
            return;
        }
        self.refresh();
    }

    /// Edit a text entry (§13) and, on "Save and copy", put the result on the
    /// clipboard — the §61 acceptance flow.
    fn edit(self: &Rc<Self>, id: i64) {
        // Load and drop the borrow before the dialog's nested main loop.
        let loaded = {
            let store = self.deps.store.borrow();
            store.get(id).ok().flatten().map(|item| {
                let text = store.full_text(id).unwrap_or_else(|_| item.preview.clone());
                (item, text)
            })
        };
        let Some((item, text)) = loaded else {
            return;
        };

        let outcome = {
            let _modal = self.modal();
            dialogs::edit_text(&self.window, &item, &text)
        };
        let (edited, also_copy) = match outcome {
            dialogs::EditOutcome::Save(text) => (text, false),
            dialogs::EditOutcome::SaveAndCopy(text) => (text, true),
            dialogs::EditOutcome::Cancel => return,
        };

        if edited == text {
            // Nothing changed; still honour an explicit copy.
            if also_copy {
                self.restore(id, true);
            }
            return;
        }

        let preview_len = self.settings().appearance.preview_length as usize;
        if let Err(err) = self.deps.store.borrow_mut().update_text(id, &edited, preview_len) {
            tracing::error!(id, error = %err, "could not save the edited entry");
            let _modal = self.modal();
            dialogs::error(
                Some(&self.window),
                "The edit could not be saved",
                "The history database rejected the change. The original entry is intact.",
            );
            return;
        }

        if also_copy {
            // Re-read, because saving changed the content hash and the
            // suppression has to match the new one.
            self.restore(id, true);
        } else {
            self.refresh();
        }
    }

    fn view(self: &Rc<Self>, id: i64) {
        let loaded = {
            let store = self.deps.store.borrow();
            store.payload_of(id)
        };
        let Ok((item, payload)) = loaded else {
            let _modal = self.modal();
            dialogs::error(
                Some(&self.window),
                "That entry could not be read",
                "Its stored data is missing or unreadable.",
            );
            return;
        };
        let _modal = self.modal();
        dialogs::view_content(&self.window, &item, &payload);
    }

    fn delete(self: &Rc<Self>, id: i64) {
        if let Err(err) = self.deps.store.borrow_mut().delete(id) {
            tracing::error!(id, error = %err, "could not delete the entry");
            return;
        }
        self.invalidate_thumbnails();
        self.refresh();
    }

    /// "Clear all", behind a confirmation (§14, §36).
    fn clear_all(self: &Rc<Self>) {
        let pinned_count = {
            let store = self.deps.store.borrow();
            store.pinned().map(|p| p.len()).unwrap_or(0)
        };

        let body = if pinned_count == 0 {
            "Every entry in the history will be deleted. This cannot be undone.".to_string()
        } else if pinned_count == 1 {
            "Every unpinned entry will be deleted. Your 1 pinned entry will be kept. \
             This cannot be undone."
                .to_string()
        } else {
            format!(
                "Every unpinned entry will be deleted. Your {pinned_count} pinned entries \
                 will be kept. This cannot be undone."
            )
        };

        let confirmed = {
            let _modal = self.modal();
            dialogs::confirm(&self.window, "Clear clipboard history?", &body, "Clear", true)
        };
        if !confirmed {
            return;
        }

        // Pinned entries survive: §12 says a pin outlives cleanup, and a
        // "Clear all" that silently discarded them would be a trap.
        match self.deps.store.borrow_mut().clear(false) {
            Ok(removed) => tracing::info!(removed, "history cleared"),
            Err(err) => {
                tracing::error!(error = %err, "could not clear the history");
                let _modal = self.modal();
                dialogs::error(
                    Some(&self.window),
                    "The history could not be cleared",
                    "The database reported an error. Nothing has been deleted.",
                );
                return;
            }
        }

        self.invalidate_thumbnails();
        self.refresh();
    }

    // ---- keyboard ------------------------------------------------------------

    /// Keyboard handling (§7, §40). Everything reachable by mouse is reachable
    /// from here.
    fn on_key(self: &Rc<Self>, event: &gdk::EventKey) -> glib::Propagation {
        use gdk::keys::constants as key;

        let keyval = event.keyval();
        let state = event.state();
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);

        match keyval {
            key::Escape => {
                self.hide();
                glib::Propagation::Stop
            }
            // Ctrl+F is the documented way back to the search field.
            key::f | key::F if ctrl => {
                self.search.grab_focus();
                glib::Propagation::Stop
            }
            // From the search field, Down moves into the list rather than
            // moving the text cursor.
            key::Down if self.search.has_focus() => {
                self.focus_list();
                glib::Propagation::Stop
            }
            key::Delete if !self.search.has_focus() => {
                if let Some(id) = self.selected_id() {
                    self.delete(id);
                }
                glib::Propagation::Stop
            }
            // Ctrl+Delete works from the search field too, where plain Delete
            // has to keep editing text.
            key::Delete if ctrl => {
                if let Some(id) = self.selected_id() {
                    self.delete(id);
                }
                glib::Propagation::Stop
            }
            key::p | key::P if ctrl => {
                if let Some(id) = self.selected_id() {
                    self.toggle_pin(id);
                }
                glib::Propagation::Stop
            }
            key::Return | key::KP_Enter if self.search.has_focus() => {
                // Enter in the search field activates the top match, which is
                // the whole point of type-to-filter.
                if let Some(id) = self.selected_id() {
                    self.restore(id, true);
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    fn focus_list(&self) {
        let shown = self.shown.borrow();
        if let Some(index) = shown.iter().position(|id| *id != SECTION_SENTINEL) {
            if let Some(row) = self.list.row_at_index(index as i32) {
                self.list.select_row(Some(&row));
                row.grab_focus();
            }
        }
    }

    /// Id of the selected row, skipping section headings.
    fn selected_id(&self) -> Option<i64> {
        let row = self.list.selected_row()?;
        let index = row.index();
        self.shown
            .borrow()
            .get(index as usize)
            .copied()
            .filter(|id| *id != SECTION_SENTINEL)
    }

    pub fn window(&self) -> &gtk::Window {
        &self.window
    }

    /// Mark a modal dialog as open for as long as the returned guard lives.
    ///
    /// Every `dialogs::*` call from this module must be wrapped in this, or the
    /// panel hides itself out from under the dialog and the UI locks up. RAII
    /// rather than a set/clear pair because these paths return early all over
    /// the place, and a GTK signal handler can panic.
    fn modal(&self) -> ModalGuard<'_> {
        self.modal_depth.set(self.modal_depth.get() + 1);
        ModalGuard { depth: &self.modal_depth }
    }

    /// Wire up the settings button. Called once by the daemon after it has
    /// built both windows.
    pub fn set_on_open_settings(&self, open: Rc<dyn Fn()>) {
        *self.on_open_settings.borrow_mut() = open;
    }

    /// Run the "Clear all" flow, including its confirmation.
    ///
    /// Public so the tray's "Clear History…" item goes through exactly the same
    /// confirmation as the panel's own button, rather than a second copy of the
    /// wording that could drift.
    pub fn request_clear_all(self: &Rc<Self>) {
        self.clear_all();
    }

    /// Show the About dialog with the live session, backend and any platform
    /// limitation (§37).
    pub fn show_about(&self) {
        let _modal = self.modal();
        dialogs::about(
            Some(&self.window),
            &self.deps.session,
            &self.deps.backend,
            self.deps.caveat.as_deref(),
        );
    }
}

/// Keeps [`Popup::modal_depth`] raised while a dialog is open.
struct ModalGuard<'a> {
    depth: &'a Cell<u32>,
}

impl Drop for ModalGuard<'_> {
    fn drop(&mut self) {
        // `saturating_sub` so an unbalanced drop can never wrap to u32::MAX and
        // leave the panel permanently unable to dismiss itself.
        self.depth.set(self.depth.get().saturating_sub(1));
    }
}

/// Give the window an alpha channel so the CSS rounded corners and shadow are
/// not drawn against an opaque rectangle.
///
/// Only when a compositor is actually running: without one, a transparent
/// window background renders as black, which would look far worse than square
/// corners. The stylesheet branches on `cm-no-composite` to drop the shadow
/// margin in that case, so the panel still looks deliberate.
fn enable_transparency(window: &gtk::Window) {
    // Disambiguated: `screen()` exists on several traits in GTK's prelude.
    let Some(screen) = gtk::prelude::GtkWindowExt::screen(window) else {
        return;
    };
    if !screen.is_composited() {
        window.style_context().add_class("cm-no-composite");
        return;
    }
    if let Some(visual) = screen.rgba_visual() {
        window.set_visual(Some(&visual));
        window.set_app_paintable(true);
        window.style_context().add_class("cm-popup");
    }
}

// No unit tests here. This module is GTK widget construction and signal
// wiring, which needs a display to exercise and asserts nothing useful
// without one. The numeric invariants are compile-time assertions above; the
// behaviour it drives -- search, restore, pin, delete, clear -- is covered by
// `tests/pipeline.rs` against the real store.
