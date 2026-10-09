//! The settings window (§37).
//!
//! Five tabs — General, Clipboard, Privacy, Appearance, Advanced — matching the
//! structure of [`crate::config::Settings`] so that there is a one-to-one map
//! between what the user sees and what is written to `config.toml`.
//!
//! # How changes are applied
//!
//! Every control writes straight through to the shared settings and saves the
//! file. There is no OK/Cancel pair, which is deliberate: an Apply button
//! invites the "did that take effect?" question, and the controls here are all
//! individually reversible. The one exception is the destructive pair in
//! Advanced (reset the database, import), which confirm first.
//!
//! The settings are shared with the monitor thread through
//! [`crate::clipboard::SharedSettings`], so a change to "store images" or the
//! exclusion list takes effect on the very next clipping without a restart.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::clipboard::SharedSettings;
use crate::config::{ActivateAction, Appearance, ClipboardSettings, Settings, Theme};
use crate::storage::Store;
use crate::ui::{dialogs, theme};

/// What the settings window needs to do its job.
pub struct Deps {
    pub settings: SharedSettings,
    pub store: Rc<RefCell<Store>>,
    /// Re-read settings and rebuild the popup, so appearance changes show at
    /// once rather than on next open.
    pub on_changed: Rc<dyn Fn()>,
    /// Re-register the global shortcut after the accelerator is edited.
    pub on_shortcut_changed: Rc<dyn Fn(String)>,
    pub session: String,
    pub backend: String,
    /// Platform limitation from the monitoring backend, for the About dialog.
    pub caveat: Option<String>,
}

pub struct SettingsWindow {
    window: gtk::Window,
    deps: Deps,
    /// Suppresses the write-back while controls are being populated, so
    /// building the UI does not look like a hundred user edits.
    loading: std::cell::Cell<bool>,
}

impl SettingsWindow {
    pub fn new(deps: Deps) -> Rc<Self> {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Clipboard Manager Settings");
        window.set_default_size(620, 560);
        window.set_icon_name(Some("edit-paste"));

        let this = Rc::new(Self { window, deps, loading: std::cell::Cell::new(true) });
        this.build();

        // Closing the settings window must not end the daemon.
        this.window.connect_delete_event(|window, _| {
            window.hide();
            glib::Propagation::Stop
        });

        this.loading.set(false);
        this
    }

    pub fn present(&self) {
        let settings = self.read();
        theme::apply(&self.window, theme::resolve(settings.appearance.theme));
        self.window.show_all();
        self.window.present();
    }

    pub fn window(&self) -> &gtk::Window {
        &self.window
    }

    fn read(&self) -> Settings {
        match self.deps.settings.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Mutate the settings, persist them, and tell the rest of the app.
    ///
    /// Saving on every change rather than on close means a crash cannot lose
    /// the user's preferences, and the file is tiny so the cost is nil.
    fn update(&self, edit: impl FnOnce(&mut Settings)) {
        if self.loading.get() {
            return;
        }

        let saved = {
            let mut guard = match self.deps.settings.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            edit(&mut guard);
            *guard = guard.clone().normalized();
            guard.save()
        };

        if let Err(err) = saved {
            tracing::error!(error = %err, "could not save settings");
            dialogs::error(
                Some(&self.window),
                "Settings could not be saved",
                "The change is active for this session but will not survive a restart. \
                 Check that the configuration directory is writable.",
            );
        }
        (self.deps.on_changed)();
    }

    fn build(self: &Rc<Self>) {
        let notebook = gtk::Notebook::new();
        notebook.set_margin_top(8);
        notebook.set_margin_bottom(8);
        notebook.set_margin_start(8);
        notebook.set_margin_end(8);

        notebook.append_page(&self.general_tab(), Some(&gtk::Label::new(Some("General"))));
        notebook.append_page(&self.clipboard_tab(), Some(&gtk::Label::new(Some("Clipboard"))));
        notebook.append_page(&self.privacy_tab(), Some(&gtk::Label::new(Some("Privacy"))));
        notebook.append_page(&self.appearance_tab(), Some(&gtk::Label::new(Some("Appearance"))));
        notebook.append_page(&self.advanced_tab(), Some(&gtk::Label::new(Some("Advanced"))));

        self.window.add(&notebook);
    }

    // ---- General -------------------------------------------------------------

    fn general_tab(self: &Rc<Self>) -> gtk::Widget {
        let settings = self.read();
        let page = Page::new();

        page.section("Startup");
        page.add_switch(
            "Start on login",
            "Run the clipboard manager automatically when you log in.",
            settings.general.start_on_login,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.general.start_on_login = on)
            },
        );
        page.add_switch(
            "Start without showing the panel",
            "Begin recording history in the background instead of opening the panel.",
            settings.general.launch_minimized,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.general.launch_minimized = on)
            },
        );
        page.add_switch(
            "Show tray icon",
            "Where the desktop supports a system tray. Not all do — the shortcut \
             always works regardless.",
            settings.general.show_tray_icon,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.general.show_tray_icon = on)
            },
        );

        page.section("Shortcut");
        let entry = gtk::Entry::new();
        entry.set_text(&settings.general.shortcut);
        entry.set_width_chars(18);
        entry.set_tooltip_text(Some(
            "For example: Ctrl+grave, Super+V, Ctrl+Shift+V. \
             Press Return to apply.",
        ));
        {
            let this = Rc::clone(self);
            entry.connect_activate(move |entry| {
                let text = entry.text().to_string();
                this.update(|s| s.general.shortcut = text.clone());
                (this.deps.on_shortcut_changed)(text);
            });
        }
        page.add_control(
            "Open clipboard history",
            &format!(
                "The default is Ctrl+` (Ctrl and the backtick key, above Tab). \
                 If the desktop refuses to register it, bind `clipboard-manager toggle` \
                 to a key yourself — the current default is {}.",
                crate::config::DEFAULT_SHORTCUT
            ),
            &entry,
        );

        page.into_widget()
    }

    // ---- Clipboard -----------------------------------------------------------

    fn clipboard_tab(self: &Rc<Self>) -> gtk::Widget {
        let settings = self.read();
        let page = Page::new();

        page.section("History");

        let history = combo_from_u32(&ClipboardSettings::HISTORY_CHOICES, settings.clipboard.max_history);
        {
            let this = Rc::clone(self);
            let choices = ClipboardSettings::HISTORY_CHOICES;
            history.connect_changed(move |combo| {
                if let Some(value) = selected_value(combo, &choices) {
                    this.update(|s| s.clipboard.max_history = value);
                }
            });
        }
        page.add_control(
            "Keep at most",
            "Pinned entries are never counted against this limit or removed by it.",
            &history,
        );

        let retention =
            combo_from_u32(&ClipboardSettings::RETENTION_CHOICES, settings.clipboard.retention_days);
        {
            let this = Rc::clone(self);
            let choices = ClipboardSettings::RETENTION_CHOICES;
            retention.connect_changed(move |combo| {
                if let Some(value) = selected_value(combo, &choices) {
                    this.update(|s| s.clipboard.retention_days = value);
                }
            });
        }
        page.add_control("Delete entries older than", "Pinned entries never expire.", &retention);

        let size = combo_from_u64(&ClipboardSettings::SIZE_CHOICES, settings.clipboard.max_item_size_mb);
        {
            let this = Rc::clone(self);
            let choices = ClipboardSettings::SIZE_CHOICES;
            size.connect_changed(move |combo| {
                if let Some(value) = selected_value_u64(combo, &choices) {
                    this.update(|s| s.clipboard.max_item_size_mb = value);
                }
            });
        }
        page.add_control(
            "Skip items larger than",
            "A larger item is not recorded, and you are notified once. \
             \"Unlimited\" still has a 2 GB ceiling, because the data has to fit in memory.",
            &size,
        );

        page.section("What to record");
        page.add_switch("Store images", "Screenshots and copied pictures.", settings.clipboard.store_images, {
            let this = Rc::clone(self);
            move |on| this.update(|s| s.clipboard.store_images = on)
        });
        page.add_switch(
            "Store files",
            "File and folder selections copied from a file manager.",
            settings.clipboard.store_files,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.clipboard.store_files = on)
            },
        );
        page.add_switch(
            "Ignore duplicates",
            "Copying the same thing again moves the existing entry to the top instead \
             of adding a second copy.",
            settings.clipboard.ignore_duplicates,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.clipboard.ignore_duplicates = on)
            },
        );
        page.add_switch(
            "Trim trailing whitespace",
            "Remove trailing spaces and newlines from copied text before storing it.",
            settings.clipboard.trim_whitespace,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.clipboard.trim_whitespace = on)
            },
        );
        page.add_switch(
            "Also record the middle-click selection",
            "On X11, records text as soon as you select it (the PRIMARY selection). \
             Off by default because it changes on every drag and floods the history.",
            settings.clipboard.monitor_primary,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.clipboard.monitor_primary = on)
            },
        );

        page.into_widget()
    }

    // ---- Privacy -------------------------------------------------------------

    fn privacy_tab(self: &Rc<Self>) -> gtk::Widget {
        let settings = self.read();
        let page = Page::new();

        page.section("Recording");
        page.add_switch(
            "Pause monitoring",
            "Stop recording new entries. Existing history stays readable.",
            settings.privacy.paused,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.privacy.paused = on)
            },
        );
        page.add_switch(
            "Clear history when the application exits",
            "Unpinned entries are deleted on a clean shutdown.",
            settings.privacy.clear_on_exit,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.privacy.clear_on_exit = on)
            },
        );
        page.add_switch(
            "…including pinned entries",
            "Only applies when the option above is on.",
            settings.privacy.clear_pinned_on_exit,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.privacy.clear_pinned_on_exit = on)
            },
        );

        page.section("Passwords and secrets");
        page.add_switch(
            "Skip entries marked as passwords",
            "Honours the hint that password managers such as KeePassXC attach to a \
             copied password. Exact and reliable.",
            settings.privacy.skip_passwords,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.privacy.skip_passwords = on)
            },
        );
        page.add_switch(
            "Also skip text that looks like a key or token",
            "A guess based on the shape of the text, so it can be wrong in both \
             directions. Off by default.",
            settings.privacy.skip_secret_like_text,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.privacy.skip_secret_like_text = on)
            },
        );

        page.section("Excluded applications");

        // The honest caveat, in the UI and not only in the docs (§16, §56).
        let note = gtk::Label::new(Some(
            "Exclusion needs to know which application owns the clipboard, which only \
             X11 reports. Under Wayland this list has no effect — the protocol carries \
             no application identity. The password hint above still works everywhere.",
        ));
        note.set_line_wrap(true);
        note.set_xalign(0.0);
        note.style_context().add_class("cm-banner");
        page.add_raw(&note);

        let (list_widget, model) = string_list(&settings.privacy.excluded_apps);
        let model = Rc::new(model);

        let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let add_entry = gtk::Entry::new();
        add_entry.set_placeholder_text(Some("Application name, e.g. keepassxc"));
        add_entry.set_hexpand(true);
        let add_button = gtk::Button::with_label("Add");
        let remove_button = gtk::Button::with_label("Remove selected");
        add_row.add(&add_entry);
        add_row.add(&add_button);

        {
            let this = Rc::clone(self);
            let model = Rc::clone(&model);
            let entry = add_entry.clone();
            let commit = move || {
                let text = entry.text().trim().to_lowercase();
                if text.is_empty() {
                    return;
                }
                entry.set_text("");
                model.set(&model.append(), &[(0, &text)]);
                this.update(|s| {
                    if !s.privacy.excluded_apps.iter().any(|a| a == &text) {
                        s.privacy.excluded_apps.push(text.clone());
                    }
                });
            };
            let commit2 = commit.clone();
            add_button.connect_clicked(move |_| commit());
            add_entry.connect_activate(move |_| commit2());
        }

        {
            let this = Rc::clone(self);
            let model = Rc::clone(&model);
            let view = list_widget.clone();
            remove_button.connect_clicked(move |_| {
                let Some((selection_model, iter)) = view.selection().selected() else {
                    return;
                };
                let value: String =
                    selection_model.value(&iter, 0).get().unwrap_or_default();
                model.remove(&iter);
                this.update(|s| s.privacy.excluded_apps.retain(|a| a != &value));
            });
        }

        let scroller = gtk::ScrolledWindow::builder()
            .height_request(140)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .shadow_type(gtk::ShadowType::In)
            .child(&list_widget)
            .build();

        page.add_raw(&scroller);
        page.add_raw(&add_row);
        page.add_raw(&remove_button);

        page.into_widget()
    }

    // ---- Appearance ----------------------------------------------------------

    fn appearance_tab(self: &Rc<Self>) -> gtk::Widget {
        let settings = self.read();
        let page = Page::new();

        page.section("Theme");
        let combo = gtk::ComboBoxText::new();
        for (index, option) in Theme::ALL.iter().enumerate() {
            combo.append(Some(&index.to_string()), option.label());
            if *option == settings.appearance.theme {
                combo.set_active(Some(index as u32));
            }
        }
        {
            let this = Rc::clone(self);
            combo.connect_changed(move |combo| {
                let Some(index) = combo.active() else { return };
                let Some(choice) = Theme::ALL.get(index as usize).copied() else { return };
                this.update(|s| s.appearance.theme = choice);
            });
        }
        page.add_control("Appearance", "\"System\" follows your desktop's light/dark setting.", &combo);

        page.section("Panel");
        let width = scale(Appearance::WIDTH_RANGE, settings.appearance.popup_width as f64, 10.0);
        {
            let this = Rc::clone(self);
            width.connect_value_changed(move |s| {
                let value = s.value() as i32;
                this.update(|st| st.appearance.popup_width = value);
            });
        }
        page.add_control("Width", "In pixels.", &width);

        let height = scale(Appearance::HEIGHT_RANGE, settings.appearance.popup_height as f64, 10.0);
        {
            let this = Rc::clone(self);
            height.connect_value_changed(move |s| {
                let value = s.value() as i32;
                this.update(|st| st.appearance.popup_height = value);
            });
        }
        page.add_control("Height", "In pixels.", &height);

        let preview = scale(
            (Appearance::PREVIEW_RANGE.0 as i32, Appearance::PREVIEW_RANGE.1 as i32),
            settings.appearance.preview_length as f64,
            20.0,
        );
        {
            let this = Rc::clone(self);
            preview.connect_value_changed(move |s| {
                let value = s.value() as u32;
                this.update(|st| st.appearance.preview_length = value);
            });
        }
        page.add_control(
            "Preview length",
            "Characters of text kept for each row. Applies to entries recorded from now on.",
            &preview,
        );

        page.add_switch(
            "Show timestamps",
            "Add \"2 min ago\" to each row.",
            settings.appearance.show_timestamps,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.appearance.show_timestamps = on)
            },
        );

        page.add_switch(
            "Close when clicking outside",
            "Off by default because the panel cannot reliably keep keyboard focus on \
             some Wayland desktops, where this caused it to close itself a moment after \
             opening. Escape, choosing an entry, or pressing the shortcut again always \
             close it.",
            settings.appearance.close_on_focus_loss,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.appearance.close_on_focus_loss = on)
            },
        );

        page.section("Behaviour");
        let activate = gtk::ComboBoxText::new();
        activate.append(Some("close"), "Copy and close the panel");
        activate.append(Some("stay"), "Copy and keep the panel open");
        activate.set_active_id(Some(match settings.advanced.activate_action {
            ActivateAction::CopyAndClose => "close",
            ActivateAction::CopyAndStayOpen => "stay",
        }));
        {
            let this = Rc::clone(self);
            activate.connect_changed(move |combo| {
                let action = match combo.active_id().as_deref() {
                    Some("stay") => ActivateAction::CopyAndStayOpen,
                    _ => ActivateAction::CopyAndClose,
                };
                this.update(|s| s.advanced.activate_action = action);
            });
        }
        page.add_control(
            "Choosing an entry",
            "Either way you paste with your own paste shortcut — nothing is typed into \
             other applications for you.",
            &activate,
        );

        page.into_widget()
    }

    // ---- Advanced ------------------------------------------------------------

    fn advanced_tab(self: &Rc<Self>) -> gtk::Widget {
        let settings = self.read();
        let page = Page::new();

        page.section("Storage");
        let location = gtk::Label::new(Some(
            &self.deps.store.borrow().data_dir().display().to_string(),
        ));
        location.set_xalign(0.0);
        location.set_selectable(true);
        location.set_ellipsize(pango::EllipsizeMode::Middle);
        page.add_control(
            "History is stored in",
            "The directory is created with owner-only permissions (0700), and the \
             database file 0600, so no other user on this machine can read it.",
            &location,
        );

        let stats = self.deps.store.borrow().stats();
        if let Ok(stats) = stats {
            let summary = gtk::Label::new(Some(&format!(
                "{} entries ({} pinned), {} on disk",
                stats.total_items,
                stats.pinned_items,
                crate::model::format_size(stats.total_bytes())
            )));
            summary.set_xalign(0.0);
            page.add_raw(&summary);
        }

        page.section("Diagnostics");
        page.add_switch(
            "Verbose logging",
            "Writes more detail to the journal (journalctl --user -u clipboard-manager). \
             Clipboard contents are never logged, at any level.",
            settings.advanced.debug_logging,
            {
                let this = Rc::clone(self);
                move |on| this.update(|s| s.advanced.debug_logging = on)
            },
        );

        page.section("Backup");
        let export = gtk::Button::with_label("Export history…");
        {
            let this = Rc::clone(self);
            export.connect_clicked(move |_| this.export());
        }
        page.add_control(
            "Export",
            "Writes a JSON file. It contains your clipboard history in readable form — \
             including anything sensitive you have copied — so store it somewhere safe.",
            &export,
        );

        let import = gtk::Button::with_label("Import history…");
        {
            let this = Rc::clone(self);
            import.connect_clicked(move |_| this.import());
        }
        page.add_control("Import", "Merges a previously exported file into this history.", &import);

        page.section("Danger zone");
        let reset = gtk::Button::with_label("Delete all history and start again");
        reset.style_context().add_class("destructive-action");
        {
            let this = Rc::clone(self);
            reset.connect_clicked(move |_| this.reset_database());
        }
        page.add_raw(&reset);

        page.section("About");
        let about = gtk::Button::with_label("About Clipboard Manager");
        {
            let this = Rc::clone(self);
            about.connect_clicked(move |_| {
                dialogs::about(
                    Some(&this.window),
                    &this.deps.session,
                    &this.deps.backend,
                    this.deps.caveat.as_deref(),
                );
            });
        }
        page.add_raw(&about);

        page.into_widget()
    }

    fn export(self: &Rc<Self>) {
        let chooser = gtk::FileChooserDialog::new(
            Some("Export clipboard history"),
            Some(&self.window),
            gtk::FileChooserAction::Save,
        );
        chooser.add_button("Cancel", gtk::ResponseType::Cancel);
        chooser.add_button("Export", gtk::ResponseType::Accept);
        chooser.set_current_name("clipboard-history.json");
        chooser.set_do_overwrite_confirmation(true);

        let response = chooser.run();
        let path = chooser.filename();
        unsafe { chooser.destroy() };

        if response != gtk::ResponseType::Accept {
            return;
        }
        let Some(path) = path else { return };

        // §53: the warning is part of the feature, not a footnote.
        if !dialogs::confirm(
            &self.window,
            "This file will contain sensitive data",
            "The export holds your clipboard history as readable text, including any \
             passwords, tokens or private messages you have copied. Anyone who can read \
             the file can read all of it.",
            "Export anyway",
            false,
        ) {
            return;
        }

        let result = self.deps.store.borrow().export_to_file(&path);
        match result {
            Ok(count) => {
                dialogs::error(
                    Some(&self.window),
                    "Export complete",
                    &format!("{count} entries written to {}", path.display()),
                );
            }
            Err(err) => {
                tracing::error!(error = %err, "export failed");
                dialogs::error(
                    Some(&self.window),
                    "Export failed",
                    "The file could not be written. Check that the location is writable.",
                );
            }
        }
    }

    fn import(self: &Rc<Self>) {
        let chooser = gtk::FileChooserDialog::new(
            Some("Import clipboard history"),
            Some(&self.window),
            gtk::FileChooserAction::Open,
        );
        chooser.add_button("Cancel", gtk::ResponseType::Cancel);
        chooser.add_button("Import", gtk::ResponseType::Accept);

        let response = chooser.run();
        let path = chooser.filename();
        unsafe { chooser.destroy() };

        if response != gtk::ResponseType::Accept {
            return;
        }
        let Some(path) = path else { return };

        let result = self.deps.store.borrow_mut().import_from_file(&path);
        match result {
            Ok((added, skipped, failed)) => {
                (self.deps.on_changed)();
                dialogs::error(
                    Some(&self.window),
                    "Import complete",
                    &format!(
                        "{added} entries added, {skipped} already present, {failed} could \
                         not be read."
                    ),
                );
            }
            Err(err) => {
                tracing::error!(error = %err, "import failed");
                dialogs::error(
                    Some(&self.window),
                    "Import failed",
                    "The file could not be read as a clipboard-manager export.",
                );
            }
        }
    }

    fn reset_database(self: &Rc<Self>) {
        if !dialogs::confirm(
            &self.window,
            "Delete the entire history?",
            "Every entry, including pinned ones, and every stored image and file \
             reference will be permanently deleted. This cannot be undone.",
            "Delete everything",
            true,
        ) {
            return;
        }

        let result = self.deps.store.borrow_mut().reset();
        match result {
            Ok(()) => {
                (self.deps.on_changed)();
                tracing::info!("history reset by the user");
            }
            Err(err) => {
                tracing::error!(error = %err, "reset failed");
                dialogs::error(
                    Some(&self.window),
                    "The history could not be reset",
                    "The database reported an error. Nothing has been deleted.",
                );
            }
        }
    }
}

// ---- small widget helpers ---------------------------------------------------

/// A vertically stacked settings page with section headings and labelled rows.
///
/// Exists so each tab reads as a list of settings rather than as GTK
/// boilerplate; the alternative was the same twelve lines repeated per control.
struct Page {
    container: gtk::Box,
}

impl Page {
    fn new() -> Self {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 8);
        container.set_margin_top(14);
        container.set_margin_bottom(14);
        container.set_margin_start(16);
        container.set_margin_end(16);
        Self { container }
    }

    fn section(&self, title: &str) {
        let label = gtk::Label::new(None);
        label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(title)));
        label.set_xalign(0.0);
        label.set_margin_top(8);
        self.container.add(&label);
    }

    fn add_raw(&self, widget: &impl IsA<gtk::Widget>) {
        self.container.add(widget);
    }

    /// A label/description pair on the left and a control on the right.
    fn add_control(&self, title: &str, description: &str, control: &impl IsA<gtk::Widget>) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
        text.set_hexpand(true);
        let title_label = gtk::Label::new(Some(title));
        title_label.set_xalign(0.0);
        text.add(&title_label);
        if !description.is_empty() {
            let description_label = gtk::Label::new(Some(description));
            description_label.set_xalign(0.0);
            description_label.set_line_wrap(true);
            description_label.set_max_width_chars(52);
            description_label.style_context().add_class("dim-label");
            text.add(&description_label);
        }
        row.add(&text);

        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        holder.set_valign(gtk::Align::Center);
        holder.add(control);
        row.add(&holder);

        self.container.add(&row);
    }

    fn add_switch<F>(&self, title: &str, description: &str, active: bool, on_toggle: F)
    where
        F: Fn(bool) + 'static,
    {
        let toggle = gtk::Switch::new();
        toggle.set_active(active);
        toggle.set_valign(gtk::Align::Center);
        toggle.connect_state_set(move |_, state| {
            on_toggle(state);
            glib::Propagation::Proceed
        });
        self.add_control(title, description, &toggle);
    }

    fn into_widget(self) -> gtk::Widget {
        gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&self.container)
            .build()
            .upcast()
    }
}

fn combo_from_u32(choices: &[(u32, &'static str)], current: u32) -> gtk::ComboBoxText {
    let combo = gtk::ComboBoxText::new();
    for (index, (value, label)) in choices.iter().enumerate() {
        combo.append(Some(&index.to_string()), label);
        if *value == current {
            combo.set_active(Some(index as u32));
        }
    }
    // A config file holding a value we do not offer (hand-edited, or written by
    // a newer version) must still show something rather than an empty box.
    if combo.active().is_none() && !choices.is_empty() {
        combo.set_active(Some(0));
    }
    combo
}

fn combo_from_u64(choices: &[(u64, &'static str)], current: u64) -> gtk::ComboBoxText {
    let combo = gtk::ComboBoxText::new();
    for (index, (value, label)) in choices.iter().enumerate() {
        combo.append(Some(&index.to_string()), label);
        if *value == current {
            combo.set_active(Some(index as u32));
        }
    }
    if combo.active().is_none() && !choices.is_empty() {
        combo.set_active(Some(0));
    }
    combo
}

fn selected_value(combo: &gtk::ComboBoxText, choices: &[(u32, &'static str)]) -> Option<u32> {
    let index = combo.active()? as usize;
    choices.get(index).map(|(value, _)| *value)
}

fn selected_value_u64(combo: &gtk::ComboBoxText, choices: &[(u64, &'static str)]) -> Option<u64> {
    let index = combo.active()? as usize;
    choices.get(index).map(|(value, _)| *value)
}

fn scale(range: (i32, i32), current: f64, step: f64) -> gtk::Scale {
    let scale = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        f64::from(range.0),
        f64::from(range.1),
        step,
    );
    scale.set_value(current);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_digits(0);
    scale.set_size_request(220, -1);
    scale
}

/// A single-column editable list, used for the exclusion list.
fn string_list(values: &[String]) -> (gtk::TreeView, gtk::ListStore) {
    let model = gtk::ListStore::new(&[glib::Type::STRING]);
    for value in values {
        model.set(&model.append(), &[(0, value)]);
    }

    let view = gtk::TreeView::with_model(&model);
    view.set_headers_visible(false);
    let column = gtk::TreeViewColumn::new();
    let cell = gtk::CellRendererText::new();
    gtk::prelude::CellLayoutExt::pack_start(&column, &cell, true);
    gtk::prelude::CellLayoutExt::add_attribute(&column, &cell, "text", 0);
    view.append_column(&column);

    (view, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_history_choice_round_trips_through_the_combo_index() {
        // The combo stores positions, not values; a mismatch here would mean
        // picking "500 items" silently saved something else.
        for (value, _) in ClipboardSettings::HISTORY_CHOICES {
            let index = ClipboardSettings::HISTORY_CHOICES
                .iter()
                .position(|(v, _)| *v == value)
                .expect("choice must be findable");
            assert_eq!(ClipboardSettings::HISTORY_CHOICES[index].0, value);
        }
    }

    #[test]
    fn the_size_choices_include_an_unlimited_sentinel() {
        // `0` is what `max_item_bytes()` reads as unlimited.
        assert!(ClipboardSettings::SIZE_CHOICES.iter().any(|(v, _)| *v == 0));
        assert!(ClipboardSettings::RETENTION_CHOICES.iter().any(|(v, _)| *v == 0));
    }

    #[test]
    fn the_appearance_ranges_contain_their_own_defaults() {
        // A default outside its slider's range would snap on first open.
        let defaults = Appearance::default();
        assert!(
            (Appearance::WIDTH_RANGE.0..=Appearance::WIDTH_RANGE.1)
                .contains(&defaults.popup_width)
        );
        assert!(
            (Appearance::HEIGHT_RANGE.0..=Appearance::HEIGHT_RANGE.1)
                .contains(&defaults.popup_height)
        );
        assert!(
            (Appearance::PREVIEW_RANGE.0..=Appearance::PREVIEW_RANGE.1)
                .contains(&defaults.preview_length)
        );
    }
}
