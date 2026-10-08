//! The command-line interface (§24, §54).
//!
//! ```text
//! clipboard-manager                 start the daemon (or raise the running one)
//! clipboard-manager toggle          open/close the panel  ← what shortcuts bind to
//! clipboard-manager show | hide
//! clipboard-manager clear [--include-pinned]
//! clipboard-manager pause | resume
//! clipboard-manager status
//! clipboard-manager search "ffmpeg"
//! clipboard-manager settings
//! clipboard-manager quit
//! clipboard-manager export FILE | import FILE
//! clipboard-manager install | uninstall
//! ```
//!
//! Every subcommand except `export`/`import`/`install`/`uninstall` is a thin
//! wrapper over one [`Request`] on the daemon's socket. §24 requires the CLI
//! and GUI to talk to the *same* background instance rather than each spawning
//! a monitor, and routing everything through IPC is what guarantees it: there
//! is no code path here that can open the database while the daemon holds it.
//!
//! `export` and `import` are the exceptions, and deliberately so — they are
//! useful on a history whose daemon is not running (restoring onto a new
//! machine), so they open the store directly. They refuse to do that while a
//! daemon is live, to keep the single-writer rule.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::config::{Settings, DEFAULT_SHORTCUT};
use crate::ipc::{self, Request, Response};
use crate::model;

#[derive(Debug, Parser)]
#[command(
    name = "clipboard-manager",
    version,
    about = "Clipboard history for Linux desktops",
    long_about = None,
    after_help = concat!(
        "The default shortcut to open the clipboard history is Ctrl+` \
         (Ctrl and the backtick key, above Tab).\n\
         If your desktop cannot register it automatically, bind this command \
         to a key yourself:\n\n    clipboard-manager toggle\n"
    )
)]
pub struct Cli {
    /// Log at debug level. Clipboard contents are never logged, at any level.
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show the panel if hidden, hide it if shown. Bind this to a key.
    Toggle,
    /// Show the clipboard history panel.
    Show,
    /// Hide the clipboard history panel.
    Hide,
    /// Delete clipboard history.
    Clear(ClearArgs),
    /// Stop recording new clipboard entries.
    Pause,
    /// Start recording clipboard entries again.
    Resume,
    /// Print what the daemon is doing.
    Status,
    /// Search the history and print matching entries.
    Search(SearchArgs),
    /// Open the settings window.
    Settings,
    /// Shut the background service down.
    Quit,
    /// Write the history to a JSON file. Contains sensitive data.
    Export(FileArgs),
    /// Merge a previously exported JSON file into the history.
    Import(FileArgs),
    /// Install the autostart entry and the systemd user service.
    Install,
    /// Remove the autostart entry and the systemd user service.
    Uninstall,
}

#[derive(Debug, Args)]
pub struct ClearArgs {
    /// Also delete pinned entries. Without this they are kept.
    #[arg(long)]
    pub include_pinned: bool,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// What to look for. Case-insensitive, and matches inside entries.
    pub query: String,
    /// Maximum number of results.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct FileArgs {
    /// Path to the JSON file.
    pub file: std::path::PathBuf,
}

/// Run a subcommand. Returns the process exit code.
pub fn run(command: Command) -> Result<i32> {
    match command {
        Command::Toggle => simple(Request::Toggle),
        Command::Show => simple(Request::Show),
        Command::Hide => simple(Request::Hide),
        Command::Settings => simple(Request::Settings),
        Command::Pause => simple(Request::Pause),
        Command::Resume => simple(Request::Resume),
        Command::Quit => simple(Request::Quit),
        Command::Clear(args) => simple(Request::Clear { include_pinned: args.include_pinned }),
        Command::Search(args) => search(args),
        Command::Status => status(),
        Command::Export(args) => export(&args.file),
        Command::Import(args) => import(&args.file),
        Command::Install => crate::install::install().map(|_| 0),
        Command::Uninstall => crate::install::uninstall().map(|_| 0),
    }
}

/// Send one request and print whatever message comes back.
fn simple(request: Request) -> Result<i32> {
    let response = ipc::send(&request)?;
    match response {
        Response::Ok { message } => {
            if let Some(message) = message {
                println!("{message}");
            }
            Ok(0)
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            Ok(1)
        }
        other => {
            // A well-formed reply of the wrong shape means the two halves are
            // out of step, which is worth saying plainly.
            eprintln!("error: unexpected reply from the daemon: {other:?}");
            Ok(1)
        }
    }
}

fn search(args: SearchArgs) -> Result<i32> {
    let response = ipc::send(&Request::Search { query: args.query.clone(), limit: args.limit })?;
    match response {
        Response::Items { items } => {
            if items.is_empty() {
                println!("No entries match \"{}\".", args.query);
                return Ok(0);
            }
            let now = model::now_ms();
            for item in items {
                // Tab-separated, so the output is usable with cut/awk while
                // still being readable (§54).
                println!(
                    "{}\t{}{}\t{}\t{}",
                    item.id,
                    item.kind,
                    if item.is_pinned { " (pinned)" } else { "" },
                    model::relative_time(item.last_used_at, now),
                    item.preview
                );
            }
            Ok(0)
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            Ok(1)
        }
        other => {
            eprintln!("error: unexpected reply from the daemon: {other:?}");
            Ok(1)
        }
    }
}

fn status() -> Result<i32> {
    let response = ipc::send(&Request::Status)?;
    let Response::Status(report) = response else {
        if let Response::Error { message } = response {
            eprintln!("error: {message}");
            return Ok(1);
        }
        eprintln!("error: unexpected reply from the daemon");
        return Ok(1);
    };

    println!("clipboard-manager {} (pid {})", report.version, report.pid);
    println!("  session:   {}", report.session);
    println!("  monitor:   {} ({})", report.backend, report.health);
    println!(
        "  shortcut:  {}",
        if report.shortcut_working {
            report.shortcut.clone()
        } else {
            // §3: never imply a shortcut works when it does not.
            format!("{} [NOT WORKING]", report.shortcut)
        }
    );
    println!("  recording: {}", if report.paused { "paused" } else { "yes" });
    println!(
        "  history:   {} entries ({} pinned), {}",
        report.total_items,
        report.pinned_items,
        model::format_size(report.storage_bytes)
    );
    println!("  storage:   {}", report.storage_path);
    if let Some(caveat) = &report.caveat {
        println!("\nPlatform limitation:\n  {caveat}");
    }

    // Non-zero when the shortcut is not working, so `install` scripts and the
    // user can tell without parsing the text.
    Ok(i32::from(!report.shortcut_working))
}

fn export(path: &std::path::Path) -> Result<i32> {
    if ipc::is_running() {
        bail!(
            "the background service is running and holds the database.\n\
             Use Settings → Advanced → Export, or stop it first with \
             `clipboard-manager quit`."
        );
    }

    // §53: the warning is mandatory and goes to stderr, so it is seen even when
    // stdout is redirected.
    crate::storage::maintenance::print_export_warning(std::io::stderr())
        .context("could not print the export warning")?;

    let settings = Settings::load().normalized();
    let store = crate::storage::Store::open(&settings)?;
    let count = store
        .export_to_file(path)
        .with_context(|| format!("could not write {}", path.display()))?;

    println!("{count} entries exported to {}", path.display());
    Ok(0)
}

fn import(path: &std::path::Path) -> Result<i32> {
    if ipc::is_running() {
        bail!(
            "the background service is running and holds the database.\n\
             Use Settings → Advanced → Import, or stop it first with \
             `clipboard-manager quit`."
        );
    }

    let settings = Settings::load().normalized();
    let mut store = crate::storage::Store::open(&settings)?;
    let (added, skipped, failed) = store
        .import_from_file(path)
        .with_context(|| format!("could not read {}", path.display()))?;

    println!("{added} added, {skipped} already present, {failed} unreadable");
    // Unreadable entries are worth a non-zero status without being fatal: the
    // import did happen.
    Ok(i32::from(failed > 0))
}

/// Help text appended to `--help`, naming the default shortcut (§57).
pub fn default_shortcut_note() -> String {
    format!("The configured shortcut defaults to {DEFAULT_SHORTCUT} (Ctrl + backtick).")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        // Catches duplicate flags, bad defaults and conflicting short options
        // at test time rather than when a user runs it.
        Cli::command().debug_assert();
    }

    #[test]
    fn every_subcommand_the_specification_lists_is_present() {
        // §24 names these explicitly.
        let command = Cli::command();
        let names: Vec<&str> = command.get_subcommands().map(|s| s.get_name()).collect();
        for expected in
            ["toggle", "show", "hide", "clear", "pause", "resume", "status", "search"]
        {
            assert!(names.contains(&expected), "{expected:?} missing from {names:?}");
        }
    }

    #[test]
    fn toggle_parses_with_no_arguments_because_shortcuts_invoke_it_bare() {
        let cli = Cli::try_parse_from(["clipboard-manager", "toggle"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Toggle)));
    }

    #[test]
    fn running_with_no_subcommand_means_start_the_daemon() {
        let cli = Cli::try_parse_from(["clipboard-manager"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.debug);
    }

    #[test]
    fn clear_keeps_pinned_entries_unless_explicitly_told_otherwise() {
        // §12: pins survive cleanup, so the destructive form must be opt-in.
        let cli = Cli::try_parse_from(["clipboard-manager", "clear"]).unwrap();
        match cli.command {
            Some(Command::Clear(args)) => assert!(!args.include_pinned),
            other => panic!("{other:?}"),
        }

        let cli =
            Cli::try_parse_from(["clipboard-manager", "clear", "--include-pinned"]).unwrap();
        match cli.command {
            Some(Command::Clear(args)) => assert!(args.include_pinned),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_search_example_from_the_specification_parses() {
        // §8's worked example.
        let cli = Cli::try_parse_from(["clipboard-manager", "search", "ffmpeg"]).unwrap();
        match cli.command {
            Some(Command::Search(args)) => {
                assert_eq!(args.query, "ffmpeg");
                assert_eq!(args.limit, 20);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_multi_word_query_needs_quoting_and_then_arrives_intact() {
        let cli =
            Cli::try_parse_from(["clipboard-manager", "search", "restart nginx"]).unwrap();
        match cli.command {
            Some(Command::Search(args)) => assert_eq!(args.query, "restart nginx"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_debug_flag_works_before_or_after_the_subcommand() {
        // `global = true`; without it, `clipboard-manager toggle --debug` fails.
        assert!(Cli::try_parse_from(["clipboard-manager", "--debug", "toggle"]).unwrap().debug);
        assert!(Cli::try_parse_from(["clipboard-manager", "toggle", "--debug"]).unwrap().debug);
    }

    #[test]
    fn help_names_the_default_shortcut() {
        // §57 wants it documented prominently; this is where a user looks first.
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("Ctrl+`"), "{help}");
        assert!(help.contains("backtick"), "{help}");
        assert!(default_shortcut_note().contains("Ctrl+grave"));
    }

    #[test]
    fn version_comes_from_the_package_rather_than_a_hardcoded_string() {
        let rendered = Cli::command().render_version().to_string();
        assert!(rendered.contains(env!("CARGO_PKG_VERSION")), "{rendered}");
    }
}
