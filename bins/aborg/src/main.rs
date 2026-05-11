//! `aborg` — CLI client.
//!
//! Thin layer over the daemon's HTTP API and Unix socket. Read-only
//! operations can hit the SQLite DB directly when no daemon is
//! running; write operations always go through the daemon.
//!
//! Noun-verb structure: `aborg book list`, `aborg library scan`, etc.
//! See `docs/CLI.md` (generated) for the full catalogue.

#![allow(missing_docs)]

use clap::{Parser, Subcommand};

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = ab_core::build_info::CLI_BINARY,
    version = ab_core::build_info::VERSION,
    about = ab_core::build_info::DESCRIPTION,
)]
struct Cli {
    /// Output format. `human` (default), `json`, `tsv`, `csv`, `html`.
    #[arg(short = 'o', long, global = true, default_value = "human")]
    output: String,

    /// Suppress info-level messages.
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Increase verbosity (-v: debug, -vv: trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Override the resolved config path.
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Library operations (scan, gaps, duplicates, reindex).
    Library {
        #[command(subcommand)]
        action: LibraryAction,
    },
    /// Book operations (list, show, edit, delete, retag).
    Book {
        #[command(subcommand)]
        action: BookAction,
    },
    /// Daemon control (start, stop, status, enable, disable, reload).
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Diagnostics (read-only health checks).
    Doctor,
    /// Show daemon health.
    Health,
}

#[derive(Debug, Subcommand)]
enum LibraryAction {
    /// Scan a directory and ingest its books.
    Scan {
        /// Path to scan.
        path: std::path::PathBuf,
    },
    /// Show series gaps.
    Gaps,
    /// Show audio-fingerprint duplicates.
    Duplicates,
}

#[derive(Debug, Subcommand)]
enum BookAction {
    /// List books.
    List,
    /// Show details of one book.
    Show { id: i64 },
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Start in the foreground (no `LaunchAgent` involved).
    Start,
    /// Stop a running daemon.
    Stop,
    /// Print status.
    Status,
    /// Install a `LaunchAgent` that starts the daemon at login.
    Enable,
    /// Remove the `LaunchAgent`.
    Disable,
    /// Trigger a SIGHUP-equivalent reload.
    Reload,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.quiet, cli.verbose);

    match cli.command {
        Command::Library { action } => match action {
            LibraryAction::Scan { path } => {
                tracing::info!(?path, "library.scan.start");
                eprintln_via_tracing("library scan not yet implemented in scaffold");
            }
            LibraryAction::Gaps => eprintln_via_tracing("library gaps not yet implemented"),
            LibraryAction::Duplicates => {
                eprintln_via_tracing("library duplicates not yet implemented");
            }
        },
        Command::Book { action } => match action {
            BookAction::List => eprintln_via_tracing("book list not yet implemented"),
            BookAction::Show { id } => tracing::info!(id, "book.show"),
        },
        Command::Daemon { action } => match action {
            DaemonAction::Start
            | DaemonAction::Stop
            | DaemonAction::Status
            | DaemonAction::Enable
            | DaemonAction::Disable
            | DaemonAction::Reload => {
                eprintln_via_tracing("daemon control not yet implemented in scaffold");
            }
        },
        Command::Doctor => eprintln_via_tracing("doctor not yet implemented"),
        Command::Health => eprintln_via_tracing("health not yet implemented"),
    }
    Ok(())
}

fn init_tracing(quiet: bool, verbose: u8) {
    let level = if quiet {
        "warn"
    } else {
        match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// Route messages-to-user through tracing rather than `eprintln!`
/// (which the lint config forbids). Real impl will be a typed
/// formatter that respects `-o`.
fn eprintln_via_tracing(msg: &str) {
    tracing::warn!("{msg}");
}
