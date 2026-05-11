//! `aborg` — CLI client.
//!
//! Thin layer over the daemon's HTTP API. Library scan + book list
//! land in slice 1A — the daemon is the canonical writer; this
//! binary just submits requests and pretty-prints responses.
//!
//! Noun-verb structure: `aborg book list`, `aborg library scan`, etc.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// Default URL where the local daemon listens. Override via `--daemon`.
const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:8429";

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = ab_core::build_info::CLI_BINARY,
    version = ab_core::build_info::VERSION,
    about = ab_core::build_info::DESCRIPTION,
)]
struct Cli {
    /// Output format. `human` (default), `json`.
    #[arg(short = 'o', long, global = true, default_value = "human")]
    output: OutputFormat,

    /// Suppress info-level messages.
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Increase verbosity (-v: debug, -vv: trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Override the resolved config path.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Daemon base URL.
    #[arg(long, global = true, default_value = DEFAULT_DAEMON_URL)]
    daemon: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum OutputFormat {
    Human,
    Json,
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
        path: PathBuf,
    },
    /// Show series gaps (not yet implemented).
    Gaps,
    /// Show audio-fingerprint duplicates (not yet implemented).
    Duplicates,
}

#[derive(Debug, Subcommand)]
enum BookAction {
    /// List books.
    List,
    /// Show details of one book (not yet implemented).
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
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.quiet, cli.verbose);

    match cli.command {
        Command::Library { action } => match action {
            LibraryAction::Scan { path } => library_scan(&cli.daemon, &path, cli.output).await?,
            LibraryAction::Gaps | LibraryAction::Duplicates => {
                tracing::warn!("not yet implemented");
            }
        },
        Command::Book { action } => match action {
            BookAction::List => books_list(&cli.daemon, cli.output).await?,
            BookAction::Show { id } => tracing::info!(id, "book.show: not yet implemented"),
        },
        Command::Daemon { action: _ } => {
            tracing::warn!("daemon control not yet implemented in slice 1A");
        }
        Command::Doctor => tracing::warn!("doctor not yet implemented"),
        Command::Health => health(&cli.daemon, cli.output).await?,
    }
    Ok(())
}

// ── HTTP helpers ─────────────────────────────────────────────────────

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!(
            "{}/{}",
            ab_core::build_info::CLI_BINARY,
            ab_core::build_info::VERSION
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Serialize)]
struct ScanRequest {
    path: PathBuf,
}

#[derive(Deserialize, Debug, Serialize)]
struct ScanResponse {
    new_book_ids: Vec<i64>,
    skipped_paths: Vec<String>,
    non_audio_count: u64,
    total_walked: u64,
}

async fn library_scan(daemon: &str, path: &std::path::Path, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/library/scan");
    let resp = client()
        .post(&url)
        .json(&ScanRequest {
            path: path.to_path_buf(),
        })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("scan failed: HTTP {status}: {body}");
    }
    let report: ScanResponse = resp.json().await.context("parse scan response")?;

    match output {
        OutputFormat::Json => {
            tracing::info!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            tracing::info!(
                new = report.new_book_ids.len(),
                skipped = report.skipped_paths.len(),
                non_audio = report.non_audio_count,
                walked = report.total_walked,
                "scan complete"
            );
        }
    }
    Ok(())
}

#[derive(Deserialize, Debug, Serialize)]
struct BookRow {
    book_id: i64,
    title: String,
    file_path: Option<String>,
}

#[derive(Deserialize, Debug, Serialize)]
struct BooksResponse {
    books: Vec<BookRow>,
}

async fn books_list(daemon: &str, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/books");
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("books list failed: HTTP {status}: {body}");
    }
    let body: BooksResponse = resp.json().await.context("parse books response")?;

    match output {
        OutputFormat::Json => {
            tracing::info!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            if body.books.is_empty() {
                tracing::info!("no books");
            } else {
                for b in &body.books {
                    let file = b.file_path.as_deref().unwrap_or("<no file>");
                    tracing::info!(book_id = b.book_id, title = %b.title, file = %file, "book");
                }
                tracing::info!(count = body.books.len(), "total");
            }
        }
    }
    Ok(())
}

#[derive(Deserialize, Debug, Serialize)]
struct HealthResponse {
    status: String,
    uptime_secs: u64,
    app: String,
    version: String,
}

async fn health(daemon: &str, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/health");
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("health check failed: HTTP {status}");
    }
    let body: HealthResponse = resp.json().await.context("parse health response")?;
    match output {
        OutputFormat::Json => {
            tracing::info!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            tracing::info!(
                app = %body.app,
                version = %body.version,
                uptime_secs = body.uptime_secs,
                "daemon is {}",
                body.status
            );
        }
    }
    Ok(())
}

// ── Tracing setup ────────────────────────────────────────────────────

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
