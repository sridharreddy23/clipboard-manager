//! Entry point.
//!
//! Two modes, decided by whether a subcommand was given:
//!
//! * no subcommand — become the daemon: take the single-instance lock, open the
//!   history, start monitoring, register the shortcut and run the GTK main loop.
//!   If a daemon already holds the lock, raise its panel and exit (§25).
//! * a subcommand — send one request over the daemon's socket, print the reply
//!   and exit. No GTK, no database (§24).
//!
//! The split matters because `clipboard-manager toggle` is what desktop
//! shortcuts invoke, possibly several times a second if a key is held. It must
//! be cheap: no toolkit initialisation, no database open, just a connect and a
//! write.

use std::process::ExitCode;

use clap::Parser;
use clipboard_manager::cli::{Cli, Command};
use clipboard_manager::{daemon, ipc, logging};

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Some(command) => run_cli(command, cli.debug),
        None => run_daemon(cli.debug),
    }
}

/// One-shot client mode.
fn run_cli(command: Command, debug: bool) -> ExitCode {
    // A different logging setup: the CLI's output is for a person reading a
    // terminal, so diagnostics must not interleave with it.
    logging::init_cli(debug);

    match clipboard_manager::cli::run(command) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => {
            // `{err:#}` prints the whole anyhow chain, which is where the
            // actionable part usually is ("no daemon is running", and why).
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Daemon mode.
fn run_daemon(debug: bool) -> ExitCode {
    // Read settings once here, before logging, so `debug_logging` from the
    // config file is honoured as well as `--debug`.
    let settings = clipboard_manager::config::Settings::load();
    logging::init(debug || settings.advanced.debug_logging);

    // Another instance already has the lock: raise its panel instead of
    // starting a rival monitor (§25). Checked before GTK init so the second
    // launch costs almost nothing.
    if ipc::is_running() {
        tracing::info!("another instance is already running; raising its window");
        match ipc::send(&ipc::Request::Show) {
            Ok(_) => return ExitCode::SUCCESS,
            Err(err) => {
                // The socket answered `Ping` a moment ago and has now gone,
                // so the incumbent is shutting down. Carrying on to take the
                // lock ourselves is the right move; if it has not finished,
                // `Core::start` reports the conflict properly.
                tracing::warn!(error = %err, "the running instance stopped responding");
            }
        }
    }

    if let Err(err) = gtk::init() {
        eprintln!(
            "error: could not connect to a graphical session: {err}\n\
             clipboard-manager needs a running desktop session. If you are trying to \
             control a daemon from a terminal, use a subcommand such as \
             `clipboard-manager status`."
        );
        return ExitCode::FAILURE;
    }

    // The stylesheet goes on before any window is built, so nothing flashes
    // unstyled.
    clipboard_manager::ui::theme::install();

    let core = match daemon::Core::start() {
        Ok(core) => core,
        Err(err) => {
            eprintln!("error: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    // Keep the autostart entry matching the setting on every start, so toggling
    // it in Settings needs no separate install step (§42).
    clipboard_manager::install::sync_autostart(settings.general.start_on_login);

    install_signal_handlers(&core);

    if !settings.general.launch_minimized {
        core.popup().show();
    }

    tracing::info!("ready");
    gtk::main();
    tracing::info!("stopped");
    ExitCode::SUCCESS
}

/// Shut down cleanly on SIGINT and SIGTERM (§49).
///
/// Clean matters here: "clear history on exit" is a privacy setting, and a
/// daemon killed without running it would leave history the user asked to have
/// deleted. `glib::unix_signal_add_local` delivers the signal on the main loop,
/// so the handler can touch the database and GTK safely — unlike a real signal
/// handler, which could not.
fn install_signal_handlers(core: &std::rc::Rc<daemon::Core>) {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let core = std::rc::Rc::clone(core);
        glib::unix_signal_add_local(signal, move || {
            tracing::info!(signal, "received a termination signal");
            core.shutdown();
            glib::ControlFlow::Break
        });
    }
}
