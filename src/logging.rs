//! Logging setup (§36).
//!
//! Output goes to stderr, which is the correct destination for a systemd user
//! service: journald captures it, so `journalctl --user -u clipboard-manager`
//! works with no extra wiring and no journald-specific dependency.
//!
//! # Clipboard contents are never logged
//!
//! This is enforced structurally rather than by convention:
//!
//! * [`crate::model::RawClip`] and [`crate::model::NewClip`] have hand-written
//!   `Debug` impls that omit the payload, so even `tracing::debug!(?clip)`
//!   cannot print it.
//! * `ClipItem`'s derived `Debug` *does* include content, so it is never passed
//!   to a logging macro; call sites log `item.id`, `item.kind` and
//!   [`crate::model::short_hash`] instead.
//! * `--debug` changes verbosity only. There is no log level at which payloads
//!   are emitted.

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

/// Initialise the global subscriber.
///
/// `debug` comes from `--debug` or `advanced.debug_logging`. `RUST_LOG`, when
/// set, overrides both so that a targeted filter such as
/// `RUST_LOG=clipboard_manager::platform=trace` is possible.
pub fn init(debug: bool) {
    let default = if debug {
        "clipboard_manager=debug,warn"
    } else {
        "clipboard_manager=info,warn"
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    // Under journald every line is already stamped and the output is not a
    // terminal, so we drop our own timestamps and ANSI colour there and keep
    // them when a developer runs the binary directly.
    let interactive = std::io::stderr().is_terminal();

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(debug)
        .with_ansi(interactive);

    if interactive {
        // `tracing-subscriber`'s default `SystemTime` timer, which needs no
        // extra crate features.
        builder.init();
    } else {
        builder.without_time().init();
    }
}

/// Initialise a subscriber for one-shot CLI invocations.
///
/// CLI commands print their results on stdout; their diagnostics should be
/// terse and must not interleave with that output, so only warnings and errors
/// are shown unless `--debug` was passed.
pub fn init_cli(debug: bool) {
    let default = if debug {
        "clipboard_manager=debug,warn"
    } else {
        "clipboard_manager=warn"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}
