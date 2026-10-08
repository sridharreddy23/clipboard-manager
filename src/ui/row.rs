//! One entry in the history list (§32, §13, §40).
//!
//! Layout mirrors the Windows flyout: a leading type indicator (an icon, or a
//! thumbnail for images), a two-line label block, and trailing actions that
//! stay invisible until the row is hovered or focused so the list reads as
//! content rather than as a grid of buttons.
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ ┌────┐  sudo systemctl restart nginx    📌 ⋯ │
//! │ │ 🗎 │  Text · 2 min ago                     │
//! │ └────┘                                      │
//! └──────────────────────────────────────────────┘
//! ```
//!
//! The row is deliberately passive: it renders a [`ClipItem`] and reports what
//! the user did through a single [`RowAction`] callback. It never touches the
//! store, the clipboard or the settings, which is what lets the popup own all
//! the policy and keeps this testable as pure layout.
//!
//! Thumbnails are *supplied* rather than loaded here. Decoding a 40 MB PNG on
//! the main thread while the user is typing in the search box would stutter
//! visibly, so the popup owns a bounded cache and hands over a ready
//! [`gdk_pixbuf::Pixbuf`] (§34, §47).

use gtk::prelude::*;

use crate::model::{ClipItem, ClipKind};

/// What the user asked for on a row.
///
/// Carries the entry id rather than the [`ClipItem`], because by the time the
/// callback runs the list may have been rebuilt and any borrowed item would be
/// stale. The id is the only thing that stays valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// Primary action: put this entry back on the clipboard (§51).
    Activate(i64),
    /// Copy without closing the popup.
    Copy(i64),
    TogglePin(i64),
    /// Open the inline editor. Text-like entries only.
    Edit(i64),
    /// Show the whole payload in a scrollable dialog.
    View(i64),
    Delete(i64),
}

impl RowAction {
    pub fn id(self) -> i64 {
        match self {
            RowAction::Activate(id)
            | RowAction::Copy(id)
            | RowAction::TogglePin(id)
            | RowAction::Edit(id)
            | RowAction::View(id)
            | RowAction::Delete(id) => id,
        }
    }
}

/// A constructed row, with the pieces the popup needs to keep hold of.
pub struct Row {
    pub widget: gtk::ListBoxRow,
    pub id: i64,
}

/// Build the row for `item`.
///
/// `thumbnail` is used only for [`ClipKind::Image`]; passing `None` falls back
/// to the type icon, which is also what happens for an image whose blob has
/// gone missing — a broken thumbnail must not cost the user the row (§35).
pub fn build<F>(
    item: &ClipItem,
    now_ms: i64,
    show_timestamps: bool,
    thumbnail: Option<&gdk_pixbuf::Pixbuf>,
    thumb_size: i32,
    on_action: F,
) -> Row
where
    F: Fn(RowAction) + Clone + 'static,
{
    let id = item.id;

    let row = gtk::ListBoxRow::new();
    row.style_context().add_class("cm-row");
    // Focusable so the whole list is navigable from the keyboard (§40).
    row.set_can_focus(true);

    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    outer.set_valign(gtk::Align::Center);

    outer.add(&leading(item, thumbnail, thumb_size));

    // ---- labels ----
    let labels = gtk::Box::new(gtk::Orientation::Vertical, 1);
    labels.set_hexpand(true);
    labels.set_valign(gtk::Align::Center);

    let title = gtk::Label::new(Some(&item.title()));
    title.style_context().add_class("cm-row-title");
    title.set_xalign(0.0);
    title.set_ellipsize(pango::EllipsizeMode::End);
    // One line: the title is already whitespace-collapsed, and letting a
    // multi-line clipping grow the row would wreck the list rhythm.
    title.set_lines(1);
    title.set_single_line_mode(true);
    labels.add(&title);

    let subtitle = gtk::Label::new(Some(&item.subtitle(now_ms, show_timestamps)));
    subtitle.style_context().add_class("cm-row-subtitle");
    subtitle.set_xalign(0.0);
    subtitle.set_ellipsize(pango::EllipsizeMode::End);
    subtitle.set_single_line_mode(true);
    labels.add(&subtitle);

    outer.add(&labels);

    // ---- trailing actions ----
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    actions.set_valign(gtk::Align::Center);

    actions.add(&pin_button(item, on_action.clone()));
    actions.add(&overflow_button(item, on_action.clone()));

    outer.add(&actions);
    row.add(&outer);

    // The full content as a tooltip, bounded so a 10 MB clipping cannot build a
    // gigantic tooltip window. Screen readers read this too, which is why it
    // carries the type and time as well as the text.
    row.set_tooltip_text(Some(&tooltip_for(item, now_ms)));

    Row { widget: row, id }
}

/// The leading indicator: a thumbnail for images, otherwise a type icon.
fn leading(
    item: &ClipItem,
    thumbnail: Option<&gdk_pixbuf::Pixbuf>,
    thumb_size: i32,
) -> gtk::Widget {
    if item.kind == ClipKind::Image {
        if let Some(pixbuf) = thumbnail {
            let image = gtk::Image::from_pixbuf(Some(pixbuf));
            image.style_context().add_class("cm-thumb");
            return image.upcast();
        }
    }

    let image = gtk::Image::from_icon_name(Some(item.kind.icon_name()), gtk::IconSize::LargeToolbar);
    // Reserve the same width a thumbnail would take, so rows with and without
    // one keep their labels on the same vertical line.
    image.set_size_request(thumb_size.min(48), thumb_size.min(48));
    image.upcast()
}

fn pin_button<F>(item: &ClipItem, on_action: F) -> gtk::Button
where
    F: Fn(RowAction) + 'static,
{
    let id = item.id;
    let pinned = item.is_pinned;

    let button = gtk::Button::new();
    button.set_relief(gtk::ReliefStyle::None);
    button.set_can_focus(true);
    let context = button.style_context();
    context.add_class("cm-row-action");
    if pinned {
        // Keeps the pin visible without hover, so pinned state is legible at a
        // glance (§12).
        context.add_class("cm-pinned");
    }

    let icon = if pinned { "starred-symbolic" } else { "non-starred-symbolic" };
    button.set_image(Some(&gtk::Image::from_icon_name(
        Some(icon),
        gtk::IconSize::Button,
    )));
    button.set_tooltip_text(Some(if pinned { "Unpin" } else { "Pin" }));

    button.connect_clicked(move |_| on_action(RowAction::TogglePin(id)));
    button
}

/// The `⋯` menu. Which items appear depends on the entry's type (§13).
fn overflow_button<F>(item: &ClipItem, on_action: F) -> gtk::MenuButton
where
    F: Fn(RowAction) + Clone + 'static,
{
    let id = item.id;

    let button = gtk::MenuButton::new();
    button.set_relief(gtk::ReliefStyle::None);
    button.style_context().add_class("cm-row-action");
    button.set_image(Some(&gtk::Image::from_icon_name(
        Some("view-more-symbolic"),
        gtk::IconSize::Button,
    )));
    button.set_tooltip_text(Some("More actions"));

    let menu = gtk::Menu::new();
    let add = |label: &str, action: RowAction| {
        let entry = gtk::MenuItem::with_label(label);
        let on_action = on_action.clone();
        entry.connect_activate(move |_| on_action(action));
        menu.append(&entry);
    };

    add("Paste to clipboard", RowAction::Activate(id));
    add("Copy", RowAction::Copy(id));
    add(if item.is_pinned { "Unpin" } else { "Pin" }, RowAction::TogglePin(id));

    // Editing only makes sense for text; offering it for a PNG would be a
    // menu item that cannot work (§56).
    if item.kind.is_editable_text() {
        add("Edit…", RowAction::Edit(id));
    }
    if item.has_more_content() {
        add("View full content…", RowAction::View(id));
    }

    menu.append(&gtk::SeparatorMenuItem::new());
    add("Delete", RowAction::Delete(id));

    menu.show_all();
    button.set_popup(Some(&menu));
    button
}

/// Tooltip text: type, age, and a bounded excerpt.
fn tooltip_for(item: &ClipItem, now_ms: i64) -> String {
    const MAX_TOOLTIP_CHARS: usize = 600;

    let header = format!(
        "{} · {}",
        item.kind.label(),
        crate::model::relative_time(item.last_used_at, now_ms)
    );

    match item.kind {
        ClipKind::Image | ClipKind::Files => format!("{}\n{}", item.title(), header),
        _ => {
            let excerpt = crate::model::truncate_chars(&item.preview, MAX_TOOLTIP_CHARS);
            if excerpt.trim().is_empty() {
                header
            } else {
                format!("{excerpt}\n\n{header}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_reports_the_entry_it_belongs_to() {
        for action in [
            RowAction::Activate(7),
            RowAction::Copy(7),
            RowAction::TogglePin(7),
            RowAction::Edit(7),
            RowAction::View(7),
            RowAction::Delete(7),
        ] {
            assert_eq!(action.id(), 7, "{action:?} lost its id");
        }
    }

    fn item(kind: ClipKind, preview: &str) -> ClipItem {
        ClipItem {
            id: 1,
            kind,
            mime_type: "text/plain;charset=utf-8".to_string(),
            content_hash: "h".to_string(),
            text_content: None,
            blob_hash: None,
            preview: preview.to_string(),
            metadata: crate::model::Metadata::default(),
            size_bytes: preview.len() as u64,
            created_at: 0,
            updated_at: 0,
            last_used_at: 0,
            is_pinned: false,
        }
    }

    #[test]
    fn a_tooltip_is_bounded_even_for_a_huge_clipping() {
        let huge = "x".repeat(50_000);
        let tooltip = tooltip_for(&item(ClipKind::Text, &huge), 0);
        // 600 chars of excerpt plus the short header, nowhere near 50k.
        assert!(tooltip.chars().count() < 800, "{} chars", tooltip.chars().count());
    }

    #[test]
    fn an_entry_with_no_preview_still_gets_a_useful_tooltip() {
        // An empty excerpt must not produce a tooltip that is just blank lines.
        let tooltip = tooltip_for(&item(ClipKind::Text, "   "), 0);
        assert!(tooltip.starts_with("Text"), "{tooltip:?}");
    }

    #[test]
    fn image_tooltips_describe_the_image_rather_than_quoting_binary() {
        let mut it = item(ClipKind::Image, "\u{fffd}\u{fffd}binary");
        it.metadata.image =
            Some(crate::model::ImageMeta { width: 1920, height: 1080, format: "png".into() });
        let tooltip = tooltip_for(&it, 0);
        assert!(tooltip.contains("1920"), "{tooltip:?}");
        assert!(!tooltip.contains("binary"), "{tooltip:?}");
    }
}
