//! clipboard-manager — Windows-style clipboard history for Linux desktops.
//!
//! The crate is split into a library and a thin binary so that the integration
//! tests under `tests/` can exercise the storage, search and clipboard layers
//! directly, without a graphical session (§43).
//!
//! Layering, from the bottom up:
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`paths`] | XDG locations and private-by-default file I/O |
//! | [`model`] | Domain types, shared by every other module |
//! | [`config`] | Persistent settings |
//! | [`logging`] | Tracing setup that structurally cannot log clipboard content |
//! | [`search`] | Query parsing, fuzzy scoring and ranking |
//! | [`storage`] | SQLite history, blob store, retention and maintenance |
//! | [`privacy`] | What must never be stored |
//! | [`clipboard`] | Monitoring, normalisation and the capture pipeline |
//!
//! Nothing below [`clipboard`] knows about the display server, and nothing
//! below [`storage`] performs any I/O outside the data directory.

pub mod clipboard;
pub mod config;
pub mod logging;
pub mod model;
pub mod paths;
pub mod privacy;
pub mod search;
pub mod storage;
