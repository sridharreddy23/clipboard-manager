//! Modal dialogs: edit, view full content, confirm, about (§13, §32).
//!
//! Each is a plain [`gtk::Dialog`] rather than a custom window, so it inherits
//! the desktop's own window decorations, keyboard handling and screen-reader
//! behaviour. They are transient for the popup, which keeps them stacked above
//! it and centred on it.
//!
//! All of these return a value rather than performing the change themselves.
//! Deciding *what the edit means* — whether to update the stored entry, put the
//! result on the clipboard, or both — is the popup's job, and keeping that out
//! of here is what makes the §61 acceptance flow ("edit
//! `sudo systemctl restart nginx` to `apache2`, then Copy puts the edited text
//! on the clipboard") a single readable function up there instead of logic
//! smeared across two files.

use gtk::prelude::*;

use crate::model::{ClipItem, ClipKind};

/// Outcome of the edit dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// Save the new text over the existing entry.
    Save(String),
    /// Save it *and* put it on the clipboard — the §61 flow.
    SaveAndCopy(String),
    Cancel,
}

/// Edit the text of an entry.
///
/// `text` is the full payload, which the caller has already loaded; this
/// function does no I/O. Only text-like entries can reach here — the row menu
/// does not offer "Edit" for an image or a file selection (§13).
pub fn edit_text(parent: &gtk::Window, item: &ClipItem, text: &str) -> EditOutcome {
    let dialog = gtk::Dialog::with_buttons(
        Some("Edit entry"),
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[],
    );
    dialog.set_default_size(560, 360);

    // Explicit response ids rather than `ResponseType::Ok`, because there are
    // two distinct affirmative actions and conflating them would lose the
    // difference between "save" and "save and copy".
    const SAVE: i32 = 1;
    const SAVE_AND_COPY: i32 = 2;

    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Save", gtk::ResponseType::Other(SAVE as u16));
    let copy = dialog.add_button("Save and copy", gtk::ResponseType::Other(SAVE_AND_COPY as u16));
    copy.style_context().add_class("suggested-action");
    copy.grab_default();

    let content = dialog.content_area();
    content.set_spacing(8);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let hint = gtk::Label::new(Some(
        "Editing changes the stored entry. \"Save and copy\" also puts the result \
         on the clipboard.",
    ));
    hint.set_xalign(0.0);
    hint.set_line_wrap(true);
    content.add(&hint);

    let view = gtk::TextView::new();
    view.set_monospace(matches!(item.kind, ClipKind::Text | ClipKind::Url));
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_accepts_tab(false);
    let buffer = view.buffer().expect("a fresh TextView always has a buffer");
    buffer.set_text(text);

    let scroller = scrolled(&view);
    content.add(&scroller);

    dialog.show_all();
    // `grab_focus` after `show_all`: a widget that is not yet realised cannot
    // take focus, so doing this earlier silently does nothing.
    view.grab_focus();

    let response = dialog.run();
    let edited = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .map(|t| t.to_string())
        .unwrap_or_default();
    unsafe { dialog.destroy() };

    match response {
        gtk::ResponseType::Other(code) if code as i32 == SAVE => EditOutcome::Save(edited),
        gtk::ResponseType::Other(code) if code as i32 == SAVE_AND_COPY => {
            EditOutcome::SaveAndCopy(edited)
        }
        _ => EditOutcome::Cancel,
    }
}

/// Show the entire payload of an entry, read-only (§32).
///
/// Used for entries whose preview is truncated, and for images, where it shows
/// the picture at full size rather than the thumbnail.
pub fn view_content(parent: &gtk::Window, item: &ClipItem, payload: &[u8]) {
    let dialog = gtk::Dialog::with_buttons(
        Some(&format!("{} · {}", item.kind.label(), crate::model::format_size(item.size_bytes))),
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[("Close", gtk::ResponseType::Close)],
    );
    dialog.set_default_size(700, 520);

    let content = dialog.content_area();
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(12);
    content.set_margin_end(12);

    match item.kind {
        ClipKind::Image => content.add(&image_view(payload)),
        _ => {
            let view = gtk::TextView::new();
            view.set_editable(false);
            view.set_monospace(true);
            view.set_wrap_mode(gtk::WrapMode::WordChar);
            if let Some(buffer) = view.buffer() {
                // Lossy rather than refusing: an entry that is almost-UTF-8
                // should still be readable, and §35 says one bad entry must not
                // break the feature.
                buffer.set_text(&String::from_utf8_lossy(payload));
            }
            content.add(&scrolled(&view));
        }
    }

    dialog.show_all();
    dialog.run();
    unsafe { dialog.destroy() };
}

/// Render image bytes, or an honest message if they cannot be decoded.
fn image_view(payload: &[u8]) -> gtk::Widget {
    match pixbuf_from_bytes(payload) {
        Some(pixbuf) => {
            let image = gtk::Image::from_pixbuf(Some(&pixbuf));
            let scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Automatic)
                .vscrollbar_policy(gtk::PolicyType::Automatic)
                .child(&image)
                .build();
            scroller.upcast()
        }
        None => {
            // Saying so beats an empty dialog that looks broken.
            let label = gtk::Label::new(Some(
                "This image cannot be displayed — the stored data is not in a format \
                 the system knows how to decode. The entry is still intact and can be \
                 pasted into an application that understands it.",
            ));
            label.set_line_wrap(true);
            label.set_max_width_chars(60);
            label.upcast()
        }
    }
}

/// Decode image bytes into a pixbuf.
///
/// Shared with the popup's thumbnail cache. Returns `None` on any failure,
/// including a truncated or hostile file: gdk-pixbuf's loaders are the same
/// ones the rest of the desktop uses, and a failure here is a normal outcome,
/// not an error to report.
pub fn pixbuf_from_bytes(bytes: &[u8]) -> Option<gdk_pixbuf::Pixbuf> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    if loader.write(bytes).is_err() {
        // Close before returning: an unclosed loader logs a GLib warning.
        let _ = loader.close();
        return None;
    }
    if loader.close().is_err() {
        return None;
    }
    loader.pixbuf()
}

/// Decode and scale in one step, for list thumbnails.
///
/// Scaling happens through `PixbufLoader::set_size` so that gdk-pixbuf can
/// downsample *while decoding* rather than allocating the full-size image
/// first. For a 6000×4000 photograph that is the difference between a 96 MB
/// allocation and a 16 KB one, on the main thread, per row (§47).
pub fn thumbnail_from_bytes(bytes: &[u8], max_edge: i32) -> Option<gdk_pixbuf::Pixbuf> {
    let loader = gdk_pixbuf::PixbufLoader::new();

    let max_edge = max_edge.max(16);
    loader.connect_size_prepared(move |loader, width, height| {
        let longest = width.max(height);
        if longest <= max_edge {
            return;
        }
        let scale = f64::from(max_edge) / f64::from(longest);
        let w = ((f64::from(width) * scale).round() as i32).max(1);
        let h = ((f64::from(height) * scale).round() as i32).max(1);
        loader.set_size(w, h);
    });

    if loader.write(bytes).is_err() {
        let _ = loader.close();
        return None;
    }
    if loader.close().is_err() {
        return None;
    }
    loader.pixbuf()
}

/// Ask a yes/no question. Returns true only on explicit confirmation.
///
/// `destructive` marks the affirmative button, which is what makes "Clear all"
/// read as dangerous rather than routine.
pub fn confirm(
    parent: &gtk::Window,
    heading: &str,
    body: &str,
    affirmative: &str,
    destructive: bool,
) -> bool {
    let dialog = gtk::MessageDialog::new(
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Question,
        gtk::ButtonsType::None,
        heading,
    );
    dialog.set_secondary_text(Some(body));
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    let yes = dialog.add_button(affirmative, gtk::ResponseType::Accept);
    yes.style_context()
        .add_class(if destructive { "destructive-action" } else { "suggested-action" });

    // Cancel is the default, so a stray Return cannot wipe the history.
    dialog.set_default_response(gtk::ResponseType::Cancel);

    let response = dialog.run();
    unsafe { dialog.destroy() };
    response == gtk::ResponseType::Accept
}

/// Report something that went wrong, without jargon.
pub fn error(parent: Option<&gtk::Window>, heading: &str, body: &str) {
    let dialog = gtk::MessageDialog::new(
        parent,
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Error,
        gtk::ButtonsType::Close,
        heading,
    );
    dialog.set_secondary_text(Some(body));
    dialog.run();
    unsafe { dialog.destroy() };
}

/// The About dialog (§37).
///
/// `caveat` is the platform limitation the monitoring backend reported, if
/// any. This is where it lives now that the panel no longer carries a
/// permanent banner — §20 requires the limitation to be stated plainly
/// somewhere the user can find it, and About is where one looks to find out
/// what the application is doing.
pub fn about(
    parent: Option<&gtk::Window>,
    session: &str,
    backend: &str,
    caveat: Option<&str>,
) {
    let dialog = gtk::AboutDialog::new();
    dialog.set_transient_for(parent);
    dialog.set_modal(true);
    dialog.set_program_name("Clipboard Manager");
    dialog.set_version(Some(env!("CARGO_PKG_VERSION")));
    let mut comments =
        format!("Clipboard history for Linux desktops.\n\nSession: {session}\nMonitoring: {backend}");
    if let Some(caveat) = caveat {
        comments.push_str(&format!("\n\nOn this system: {caveat}"));
    }
    dialog.set_comments(Some(&comments));
    dialog.set_license_type(gtk::License::Gpl30);
    dialog.set_website(Some("https://github.com/sridharreddy23/clipboard-manager"));
    dialog.set_logo_icon_name(Some("edit-paste"));
    dialog.run();
    unsafe { dialog.destroy() };
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .shadow_type(gtk::ShadowType::In)
        .child(child)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_outcomes_carry_the_edited_text_distinctly() {
        // The §61 flow depends on "save" and "save and copy" being different
        // outcomes, not one flag the caller has to guess at.
        let save = EditOutcome::Save("sudo systemctl restart apache2".into());
        let both = EditOutcome::SaveAndCopy("sudo systemctl restart apache2".into());
        assert_ne!(save, both);
        assert_ne!(save, EditOutcome::Cancel);
    }

    #[test]
    fn undecodable_image_bytes_are_rejected_rather_than_panicking() {
        // Needs no display: gdk-pixbuf decoding is independent of GTK init.
        assert!(pixbuf_from_bytes(b"not an image at all").is_none());
        assert!(pixbuf_from_bytes(&[]).is_none());
        // A PNG header with no body is the realistic truncated-blob case.
        assert!(pixbuf_from_bytes(b"\x89PNG\r\n\x1a\n").is_none());
    }

    #[test]
    fn thumbnails_are_downsampled_while_decoding() {
        // Non-square on purpose: the aspect ratio must survive, so the long
        // edge lands on the limit and the short edge comes out proportional.
        let png = png_of(200, 100);
        let thumb = thumbnail_from_bytes(&png, 64).expect("the fixture should decode");
        assert_eq!(thumb.width(), 64, "long edge should match the limit");
        assert_eq!(thumb.height(), 32, "aspect ratio should be preserved");
    }

    #[test]
    fn a_thumbnail_smaller_than_the_limit_is_left_alone() {
        // Upscaling a small clipping would only make it blurry.
        let png = png_of(40, 40);
        let thumb = thumbnail_from_bytes(&png, 512).expect("the fixture should decode");
        assert_eq!((thumb.width(), thumb.height()), (40, 40));
    }

    #[test]
    fn an_absurdly_small_requested_edge_is_clamped_to_something_visible() {
        // Guards the `max(16)` floor: a settings value of 0 or 1 must not
        // produce a one-pixel row icon.
        let png = png_of(200, 200);
        let thumb = thumbnail_from_bytes(&png, 0).expect("the fixture should decode");
        assert_eq!((thumb.width(), thumb.height()), (16, 16));
    }

    /// A valid opaque PNG of the given size, built at runtime so the test
    /// carries no binary fixture.
    fn png_of(width: i32, height: i32) -> Vec<u8> {
        let pixbuf =
            gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, width, height).unwrap();
        pixbuf.fill(0xff0000ff);
        pixbuf.save_to_bufferv("png", &[]).unwrap()
    }
}
