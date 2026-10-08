//! The GTK user interface (§5, §38, §39, §40).
//!
//! Everything in this module runs on the GTK main thread and nowhere else.
//! That is not a limitation worked around but the design: the [`crate::storage`]
//! `Store` is owned here, single-threaded, so there is no lock to contend and
//! no chance of a database write racing the UI. Work that must not block the
//! main thread — reading a 40 MB blob, hashing, writing to SQLite from a
//! clipping — happens on the monitor thread and arrives as a
//! [`crate::clipboard::MonitorEvent`].
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`theme`] | Light/dark resolution and the bundled stylesheet |
//! | [`selection`] | Putting an entry back on the clipboard |
//! | [`row`] | One entry in the history list |
//! | [`dialogs`] | Edit, view, confirm and about |
//! | [`popup`] | The history panel itself |
//! | [`settings_window`] | The five settings tabs |

pub mod dialogs;
pub mod popup;
pub mod row;
pub mod selection;
pub mod settings_window;
pub mod theme;
pub mod tray;
