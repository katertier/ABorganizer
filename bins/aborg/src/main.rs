//! `aborg` — CLI client.
//!
//! Thin layer over the daemon's HTTP API. Library scan + book list
//! land in slice 1A — the daemon is the canonical writer; this
//! binary just submits requests and pretty-prints responses.
//!
//! Noun-verb structure: `aborg book list`, `aborg library scan`, etc.

// xtask: allow_macros — `aborg` is a user-facing CLI; formatted
// table / JSON output goes via println! to stdout. tracing
// fields don't render as nicely for human-readable diagnosis.
#![allow(missing_docs, clippy::print_stdout)]

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
    /// Diagnostics + repair commands.
    Doctor {
        #[command(subcommand)]
        action: Option<DoctorAction>,
    },
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
    /// Force re-extraction of one stage for one book.
    ///
    /// Clears the matching `ai_cache` row (if any) and the
    /// `pipeline_progress` row, then submits a new background-
    /// priority job. ADR-0023. Generic across every registered
    /// pipeline stage (`tag-read`, `extract-summary-spoiler-free`,
    /// `transcribe-full`, …).
    Retry {
        /// Book ID — matches `books.book_id`.
        book_id: i64,
        /// Stage to retry — matches a registered stage name.
        /// On unknown stage the daemon returns 400 with a list
        /// of known names in the response body.
        #[arg(long)]
        stage: String,
    },
}

#[derive(Debug, Subcommand)]
enum DoctorAction {
    /// Diagnose Speech / Apple Intelligence state for the
    /// languages the library needs.
    Speech,
    /// Install on-device Speech models. Use `--language <bcp47>`
    /// for one locale, or `--all` for every locale the library
    /// needs that isn't already installed. Pre-import: use
    /// `--language` to install ahead of time.
    Install {
        /// BCP-47 primary subtag — `de`, `en`, `zh-Hans`, etc.
        /// Mutually exclusive with `--all`.
        #[arg(long, conflicts_with = "all")]
        language: Option<String>,
        /// Install every locale the library currently needs
        /// that isn't already installed.
        #[arg(long, conflicts_with = "language")]
        all: bool,
    },
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
            LibraryAction::Duplicates => library_duplicates(&cli.daemon, cli.output).await?,
            LibraryAction::Gaps => {
                tracing::warn!("gaps not yet implemented");
            }
        },
        Command::Book { action } => match action {
            BookAction::List => books_list(&cli.daemon, cli.output).await?,
            BookAction::Show { id } => tracing::info!(id, "book.show: not yet implemented"),
            BookAction::Retry { book_id, stage } => {
                book_retry(&cli.daemon, book_id, &stage, cli.output).await?;
            }
        },
        Command::Daemon { action: _ } => {
            tracing::warn!("daemon control not yet implemented in slice 1A");
        }
        Command::Doctor { action } => match action {
            // Default: print the speech diagnosis.
            None | Some(DoctorAction::Speech) => doctor_speech(&cli.daemon, cli.output).await?,
            Some(DoctorAction::Install { language, all }) => {
                doctor_speech_install(&cli.daemon, language, all, cli.output).await?;
            }
        },
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

#[derive(Serialize, Debug)]
struct RetryRequest<'a> {
    stage: &'a str,
}

#[derive(Deserialize, Debug, Serialize)]
struct RetryResponse {
    book_id: i64,
    stage: String,
    submitted_at: String,
    cache_cleared: bool,
    pipeline_progress_cleared: bool,
}

#[derive(Deserialize, Debug)]
struct UnknownStageProblem {
    detail: String,
    known_stages: Vec<String>,
}

/// `aborg book retry <book_id> --stage <stage>` — thin shim
/// over `POST /api/v1/books/{book_id}/retry`. ADR-0023.
///
/// On a 400 the daemon body carries `known_stages`; we surface
/// those to the user so a typo is recoverable. On a 404 we
/// surface the body's `detail` field. Network failures (daemon
/// down) propagate as `anyhow::Error`.
async fn book_retry(daemon: &str, book_id: i64, stage: &str, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/books/{book_id}/retry");
    let resp = client()
        .post(&url)
        .json(&RetryRequest { stage })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    if status == reqwest::StatusCode::BAD_REQUEST {
        // Try to decode the known_stages list. Surface as a
        // clean human-readable message; fall through to the
        // generic error path on a malformed body.
        let problem: UnknownStageProblem = resp
            .json()
            .await
            .context("decode 400 body for unknown-stage detail")?;
        anyhow::bail!(
            "{}\nknown stages: {}",
            problem.detail,
            problem.known_stages.join(", "),
        );
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("retry failed: HTTP {status}: {body}");
    }

    let body: RetryResponse = resp.json().await.context("parse retry response")?;
    match output {
        OutputFormat::Json => {
            tracing::info!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            tracing::info!(
                book_id = body.book_id,
                stage = %body.stage,
                cache_cleared = body.cache_cleared,
                progress_cleared = body.pipeline_progress_cleared,
                submitted_at = %body.submitted_at,
                "retry submitted",
            );
        }
    }
    Ok(())
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
struct DuplicateGroup {
    matching_offsets: u32,
    book_ids: Vec<i64>,
    titles: Vec<String>,
}

#[derive(Deserialize, Debug, Serialize)]
struct DuplicatesResponse {
    groups: Vec<DuplicateGroup>,
}

async fn library_duplicates(daemon: &str, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/library/duplicates");
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("duplicates failed: HTTP {status}: {body}");
    }
    let body: DuplicatesResponse = resp.json().await.context("parse duplicates response")?;
    match output {
        OutputFormat::Json => {
            tracing::info!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            if body.groups.is_empty() {
                tracing::info!("no duplicates");
            } else {
                for g in &body.groups {
                    let titles_joined = g.titles.join(" | ");
                    tracing::info!(
                        matching_offsets = g.matching_offsets,
                        books = ?g.book_ids,
                        "{}",
                        titles_joined
                    );
                }
                tracing::info!(group_count = body.groups.len(), "total groups");
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

// ── Doctor: speech ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DoctorSpeechLocale {
    locale: String,
    library_books: i64,
    blocked_books: i64,
    sdk_status: Option<String>,
    sdk_installed: bool,
    idle_state: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DoctorSpeechResponse {
    framework_available: bool,
    locales: Vec<DoctorSpeechLocale>,
}

async fn doctor_speech(daemon: &str, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/doctor/speech");
    let resp: DoctorSpeechResponse = client()
        .get(&url)
        .send()
        .await
        .context("fetch doctor/speech")?
        .error_for_status()?
        .json()
        .await
        .context("decode doctor/speech")?;

    if matches!(output, OutputFormat::Json) {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "framework_available": resp.framework_available,
                "locales": resp.locales.iter().map(|l| serde_json::json!({
                    "locale": l.locale,
                    "library_books": l.library_books,
                    "blocked_books": l.blocked_books,
                    "sdk_status": l.sdk_status,
                    "sdk_installed": l.sdk_installed,
                    "idle_state": l.idle_state,
                    "last_error": l.last_error,
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    // Table form.
    println!(
        "Apple Intelligence: {}",
        if resp.framework_available {
            "available"
        } else {
            "UNAVAILABLE — enable in System Settings → Privacy & Security → Apple Intelligence"
        }
    );
    if resp.locales.is_empty() {
        println!("\nNo language candidates in library yet. Use:");
        println!("  aborg doctor install --language <bcp47>");
        println!("to install a model in advance of importing books.");
        return Ok(());
    }
    println!(
        "\n{:<10} {:>5} {:>7} {:<14} {:<14}",
        "Locale", "Books", "Blocked", "SDK Status", "Idle State"
    );
    println!("{}", "-".repeat(60));
    for l in &resp.locales {
        println!(
            "{:<10} {:>5} {:>7} {:<14} {:<14}",
            l.locale,
            l.library_books,
            l.blocked_books,
            l.sdk_status.as_deref().unwrap_or("?"),
            l.idle_state.as_deref().unwrap_or("—"),
        );
        if let Some(err) = &l.last_error {
            println!("  ↳ last error: {err}");
        }
    }
    let needs_install: Vec<&DoctorSpeechLocale> =
        resp.locales.iter().filter(|l| !l.sdk_installed).collect();
    if !needs_install.is_empty() {
        println!("\n{} locale(s) need install. Run:", needs_install.len());
        println!("  aborg doctor install --all");
        println!("Or for one:");
        if let Some(first) = needs_install.first() {
            println!("  aborg doctor install --language {}", first.locale);
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
struct DoctorSpeechInstallResponse {
    installed: Vec<String>,
    already_installed: Vec<String>,
    failed: Vec<(String, String)>,
}

async fn doctor_speech_install(
    daemon: &str,
    language: Option<String>,
    all: bool,
    output: OutputFormat,
) -> Result<()> {
    if language.is_none() && !all {
        anyhow::bail!("specify --language <bcp47> or --all");
    }
    let url = format!("{daemon}/api/v1/doctor/speech/install");
    let body = language.map_or_else(
        || serde_json::json!({"all": true}),
        |lang| serde_json::json!({"locale": lang}),
    );
    let resp: DoctorSpeechInstallResponse = client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("post doctor/speech/install")?
        .error_for_status()?
        .json()
        .await
        .context("decode doctor/speech/install")?;

    if matches!(output, OutputFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    if !resp.installed.is_empty() {
        println!("Installed: {}", resp.installed.join(", "));
    }
    if !resp.already_installed.is_empty() {
        println!(
            "Already installed (skipped): {}",
            resp.already_installed.join(", ")
        );
    }
    if !resp.failed.is_empty() {
        println!("Failed:");
        for (loc, err) in &resp.failed {
            println!("  {loc}: {err}");
        }
    }
    if resp.installed.is_empty() && resp.failed.is_empty() {
        println!("Nothing to do.");
    }
    Ok(())
}
