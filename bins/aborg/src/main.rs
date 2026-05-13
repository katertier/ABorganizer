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
    /// Audiologo (publisher-jingle) operations (ADR-0024).
    Audiologos {
        #[command(subcommand)]
        action: AudiologoAction,
    },
    /// Identity-alias management (ADR-0026). Subcommands `alias`
    /// (record a new spelling) and `exalt` (set the displayed form).
    /// Kinds: author, narrator, series.
    Names {
        #[command(subcommand)]
        action: NamesAction,
    },
    /// Cleanup subsystem (ADR-0025). Categories: disk, db, queue.
    ///
    /// Without `--apply` the daemon dry-runs every registered
    /// target in the category and prints what it would free.
    /// `--apply` switches to delete mode; `--force` ignores the
    /// per-target age gate.
    Clean {
        /// One of `disk`, `db`, `queue`.
        category: String,
        /// Actually delete (default = dry-run).
        #[arg(long)]
        apply: bool,
        /// Skip per-target age gating (per-target docs spell out
        /// what this means; pairing codes: invalidates every
        /// unconsumed code regardless of `expires_at`).
        #[arg(long)]
        force: bool,
    },
    /// Show daemon health.
    Health,
}

#[derive(Debug, Subcommand)]
enum NamesAction {
    /// Record a new spelling on an existing identity row.
    ///
    /// Insert is idempotent — repeat with the same alias is a
    /// no-op. The alias is recorded with `source='manual'` and
    /// `is_prime=0`; use `aborg names exalt` to make it the
    /// displayed form. ADR-0026.
    Alias {
        /// Parent row's primary-key id (`author_id` / `narrator_id` /
        /// `series_id`). Find these via `aborg book list`'s
        /// author/narrator/series columns or future `aborg names
        /// list` (H.3.6 follow-up).
        id: i64,
        /// `author`, `narrator`, or `series`.
        #[arg(long)]
        kind: String,
        /// The new spelling.
        #[arg(long)]
        add: String,
    },
    /// Move the prime-alias flag to a given spelling.
    ///
    /// The target spelling must already exist on the row
    /// (`aborg names alias` it first if not). Demotes the
    /// current prime; promotes the target. Atomic.
    Exalt {
        /// Parent row's primary-key id.
        id: i64,
        /// `author`, `narrator`, or `series`.
        #[arg(long)]
        kind: String,
        /// The alias spelling to exalt.
        #[arg(long)]
        alias: String,
    },
}

#[derive(Debug, Subcommand)]
enum AudiologoAction {
    /// Apply a manual audiologo cut to a book.
    ///
    /// Inserts a `book_file_audiologos` row at `status='applied'`
    /// with `method='manual'`, recomputes `books.duration_ms`,
    /// and shifts the affected `chapters` rows. ADR-0024 slice 4A.
    Cut {
        /// Book ID — matches `books.book_id`.
        book_id: i64,
        /// `intro` or `outro`.
        #[arg(long)]
        kind: String,
        /// Where the jingle starts (ms from file start).
        #[arg(long)]
        jingle_start: i64,
        /// Where the jingle ends (ms from file start).
        #[arg(long)]
        jingle_end: i64,
        /// Optional silence padding inserted after the cut
        /// (defaults to `AudiologoTunables.{intro|outro}_padding_ms`).
        #[arg(long)]
        padding: Option<i64>,
        /// Sample + fingerprint the range and persist to
        /// `audiologos` so the cut is reusable across the
        /// library. Slice 4A logs the request; the actual
        /// fingerprint insert lands in slice 4B.
        #[arg(long)]
        add_fingerprint: bool,
        /// Specific `file_id` within the book (default: file[0]
        /// for intro, file[N-1] for outro).
        #[arg(long)]
        file_id: Option<i64>,
    },
    /// One-shot import of `ABtagger`'s audiologo fingerprints.
    ///
    /// Inserts rows with `verified_via='ab_tagger_import'`,
    /// `confidence=0.0`. User reviews each imported row via
    /// the slice-4E review pass before they fire on matches.
    Import {
        /// Path to the `ABtagger` export (JSON).
        path: PathBuf,
    },
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
    /// Force re-extraction of one or more stages for one book.
    ///
    /// Calls `Stage::reset()` for each requested stage (which
    /// clears the matching `ai_cache`, `book_field_provenance`,
    /// and `pipeline_progress` rows plus any stage-specific
    /// state), then submits each at background priority. The
    /// operator names the exact set; no implicit cascade.
    /// ADR-0023.
    ///
    /// Examples:
    ///
    ///   aborg book retry 42 --stage tag-read
    ///   aborg book retry 42 --stage tag-read,fingerprint
    ///   aborg book retry 42 --stage all
    Retry {
        /// Book ID — matches `books.book_id`.
        book_id: i64,
        /// Stage(s) to retry. Comma-separated list of registered
        /// stage names, OR the literal `all` for every stage.
        /// On unknown stage the daemon returns 400 with a list
        /// of known names in the response body.
        #[arg(long, value_delimiter = ',', num_args = 1.., required = true)]
        stage: Vec<String>,
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
            } // (Note: BookAction::Retry above carries `stage:
              // Vec<String>` post-H.1.6.)
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
        Command::Audiologos { action } => match action {
            AudiologoAction::Cut {
                book_id,
                kind,
                jingle_start,
                jingle_end,
                padding,
                add_fingerprint,
                file_id,
            } => {
                audiologo_cut(
                    &cli.daemon,
                    AudiologoCutArgs {
                        book_id,
                        kind: &kind,
                        jingle_start,
                        jingle_end,
                        padding,
                        add_fingerprint,
                        file_id,
                    },
                    cli.output,
                )
                .await?;
            }
            AudiologoAction::Import { path } => {
                audiologo_import(&path, cli.output).await?;
            }
        },
        Command::Names { action } => match action {
            NamesAction::Alias { id, kind, add } => {
                names_alias(&cli.daemon, id, &kind, &add, cli.output).await?;
            }
            NamesAction::Exalt { id, kind, alias } => {
                names_exalt(&cli.daemon, id, &kind, &alias, cli.output).await?;
            }
        },
        Command::Clean {
            category,
            apply,
            force,
        } => {
            clean(&cli.daemon, &category, apply, force, cli.output).await?;
        }
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
    /// Display author (post-H.3.3). `is_prime` alias from
    /// `author_aliases` if any, else `authors.name`. `None` when
    /// the book has no resolved author yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    /// Display narrator(s), comma-separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    narrators: Option<String>,
    /// Display primary series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    series: Option<String>,
}

#[derive(Deserialize, Debug, Serialize)]
struct BooksResponse {
    books: Vec<BookRow>,
}

/// Wire form of `stages`. Untagged so the operator can pass
/// either a JSON array OR the literal `"all"`.
#[derive(Serialize, Debug)]
#[serde(untagged)]
enum StagesPayload<'a> {
    /// One or more explicit stage names.
    List(&'a [String]),
    /// `"all"` — every registered stage.
    Wildcard(&'static str),
}

#[derive(Serialize, Debug)]
struct RetryRequest<'a> {
    stages: StagesPayload<'a>,
}

#[derive(Deserialize, Debug, Serialize)]
struct RetryStageResult {
    stage: String,
    reset_cleared_state: bool,
    submitted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize, Debug, Serialize)]
struct RetryResponse {
    book_id: i64,
    submitted_at: String,
    results: Vec<RetryStageResult>,
}

#[derive(Deserialize, Debug)]
struct UnknownStageProblem {
    detail: String,
    #[serde(default)]
    known_stages: Vec<String>,
}

/// `aborg book retry <book_id> --stage <s>[,<s>...|all]` —
/// thin shim over `POST /api/v1/books/{book_id}/retry`.
/// ADR-0023 (multi-stage extension landed in slice H.1.6).
///
/// The CLI accepts comma-separated names (or repeated
/// `--stage`) for a list, or the literal `all` token to expand
/// to every registered stage. On a 400 the daemon body
/// carries `known_stages` (when applicable); we surface those
/// to the user so a typo is recoverable. Network failures
/// (daemon down) propagate as `anyhow::Error`.
async fn book_retry(
    daemon: &str,
    book_id: i64,
    stage: &[String],
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/books/{book_id}/retry");
    // Detect the `all` wildcard. Accept both
    // `--stage all` and `--stage ALL`; reject mixed usage
    // (`--stage tag-read,all`) since that's ambiguous.
    let is_all = stage.iter().any(|s| s.eq_ignore_ascii_case("all"));
    if is_all && stage.len() > 1 {
        anyhow::bail!("--stage all must appear alone; got mixed list: {stage:?}");
    }
    let payload = if is_all {
        StagesPayload::Wildcard("all")
    } else {
        StagesPayload::List(stage)
    };
    let resp = client()
        .post(&url)
        .json(&RetryRequest { stages: payload })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    let status = resp.status();
    if status == reqwest::StatusCode::BAD_REQUEST {
        let problem: UnknownStageProblem = resp
            .json()
            .await
            .context("decode 400 body for unknown-stage detail")?;
        if problem.known_stages.is_empty() {
            anyhow::bail!("{}", problem.detail);
        }
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
            for r in &body.results {
                if let Some(err) = &r.error {
                    tracing::warn!(
                        book_id = body.book_id,
                        stage = %r.stage,
                        error = %err,
                        "retry stage failed"
                    );
                } else {
                    tracing::info!(
                        book_id = body.book_id,
                        stage = %r.stage,
                        cleared = r.reset_cleared_state,
                        submitted = r.submitted,
                        "retry submitted"
                    );
                }
            }
            tracing::info!(
                book_id = body.book_id,
                stages = body.results.len(),
                submitted_at = %body.submitted_at,
                "retry summary"
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
                    let author = b.author.as_deref().unwrap_or("");
                    let narrators = b.narrators.as_deref().unwrap_or("");
                    let series = b.series.as_deref().unwrap_or("");
                    tracing::info!(
                        book_id = b.book_id,
                        title = %b.title,
                        author = %author,
                        narrators = %narrators,
                        series = %series,
                        file = %file,
                        "book"
                    );
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

// ── Cleanup (slice H.2.3, ADR-0025) ──────────────────────────────────

#[derive(Deserialize, Debug, Serialize)]
struct CleanReportRow {
    category: String,
    name: String,
    items: u64,
    bytes: u64,
}

#[derive(Deserialize, Debug, Serialize)]
struct CleanRunResponse {
    category: String,
    apply: bool,
    force: bool,
    age_seconds: i64,
    targets: Vec<CleanReportRow>,
}

#[derive(Serialize, Debug)]
struct CleanRunRequest<'a> {
    category: &'a str,
    apply: bool,
    force: bool,
}

/// `aborg clean <category> [--apply] [--force]` — thin shim over
/// `POST /api/v1/clean/run`. Dry-run by default; `--apply` deletes;
/// `--force` ignores per-target age gates. ADR-0025.
async fn clean(
    daemon: &str,
    category: &str,
    apply: bool,
    force: bool,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/clean/run");
    let resp = client()
        .post(&url)
        .json(&CleanRunRequest {
            category,
            apply,
            force,
        })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("clean failed: HTTP {status}: {body}");
    }
    let body: CleanRunResponse = resp.json().await.context("parse clean response")?;
    match output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            let mode = if body.apply { "applied" } else { "dry-run" };
            let force_note = if body.force { " (forced)" } else { "" };
            let age_days = body.age_seconds / 86_400;
            println!(
                "clean {} {} {}{} — age cut-off {} d, {} target(s)",
                body.category,
                mode,
                if body.apply { "→" } else { "↦" },
                force_note,
                age_days,
                body.targets.len(),
            );
            let mut total_items: u64 = 0;
            let mut total_bytes: u64 = 0;
            for row in &body.targets {
                println!(
                    "  {:<24} {:>6} items {:>10} bytes",
                    row.name, row.items, row.bytes
                );
                total_items += row.items;
                total_bytes += row.bytes;
            }
            println!(
                "  {:<24} {:>6} items {:>10} bytes",
                "TOTAL", total_items, total_bytes
            );
            if !body.apply && (total_items > 0 || total_bytes > 0) {
                println!("\nrun with --apply to delete.");
            }
        }
    }
    Ok(())
}

// ── Names (slice H.3.4, ADR-0026) ────────────────────────────────────

#[derive(Serialize, Debug)]
struct NamesAliasRequest<'a> {
    alias: &'a str,
}

#[derive(Deserialize, Debug, Serialize)]
struct NamesActionResponse {
    kind: String,
    id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inserted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exalted: Option<bool>,
}

async fn names_alias(
    daemon: &str,
    id: i64,
    kind: &str,
    alias: &str,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/names/{kind}/{id}/alias");
    let resp = client()
        .post(&url)
        .json(&NamesAliasRequest { alias })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("alias failed: HTTP {status}: {body}");
    }
    let body: NamesActionResponse = resp.json().await.context("parse alias response")?;
    print_names_action(&body, output);
    Ok(())
}

async fn names_exalt(
    daemon: &str,
    id: i64,
    kind: &str,
    alias: &str,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/names/{kind}/{id}/exalt");
    let resp = client()
        .post(&url)
        .json(&NamesAliasRequest { alias })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("exalt failed: HTTP {status}: {body}");
    }
    let body: NamesActionResponse = resp.json().await.context("parse exalt response")?;
    print_names_action(&body, output);
    Ok(())
}

fn print_names_action(body: &NamesActionResponse, output: OutputFormat) {
    match output {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(body).unwrap_or_default());
        }
        OutputFormat::Human => {
            match body.inserted {
                Some(true) => println!("{} {}: alias added", body.kind, body.id),
                Some(false) => {
                    println!("{} {}: alias already present (no-op)", body.kind, body.id);
                }
                None => {}
            }
            match body.exalted {
                Some(true) => println!("{} {}: exalted", body.kind, body.id),
                Some(false) => {
                    println!("{} {}: alias was already prime (no-op)", body.kind, body.id);
                }
                None => {}
            }
        }
    }
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

// ── Audiologos (slice 4A) ────────────────────────────────────────

#[derive(Serialize, Debug)]
struct AudiologoCutBody<'a> {
    kind: &'a str,
    jingle_start_ms: i64,
    jingle_end_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    padding_ms: Option<i64>,
    add_fingerprint: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_id: Option<i64>,
}

#[derive(Deserialize, Debug, Serialize)]
struct AudiologoCutResponse {
    book_id: i64,
    file_id: i64,
    kind: String,
    row_id: i64,
    audiologo_id: Option<i64>,
    #[serde(default)]
    fingerprint_deferred: bool,
    chapters_shifted: i64,
    new_duration_ms: Option<i64>,
}

/// Args for `audiologo_cut`. Bundled to stay under the
/// project's 5-arg ceiling on `aborg` helpers.
struct AudiologoCutArgs<'a> {
    book_id: i64,
    kind: &'a str,
    jingle_start: i64,
    jingle_end: i64,
    padding: Option<i64>,
    add_fingerprint: bool,
    file_id: Option<i64>,
}

/// `aborg audiologos cut` — POSTs to
/// `/api/v1/books/{id}/audiologo`.
async fn audiologo_cut(
    daemon: &str,
    args: AudiologoCutArgs<'_>,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/books/{}/audiologo", args.book_id);
    let body = AudiologoCutBody {
        kind: args.kind,
        jingle_start_ms: args.jingle_start,
        jingle_end_ms: args.jingle_end,
        padding_ms: args.padding,
        add_fingerprint: args.add_fingerprint,
        file_id: args.file_id,
    };
    let resp = client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("audiologo cut failed: HTTP {status}: {text}");
    }
    let response: AudiologoCutResponse = resp.json().await.context("parse cut response")?;
    match output {
        OutputFormat::Json => {
            tracing::info!(
                "{}",
                serde_json::to_string_pretty(&response).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            tracing::info!(
                book_id = response.book_id,
                file_id = response.file_id,
                kind = %response.kind,
                row_id = response.row_id,
                chapters_shifted = response.chapters_shifted,
                new_duration_ms = ?response.new_duration_ms,
                "audiologo cut applied",
            );
            if response.fingerprint_deferred {
                tracing::warn!(
                    "fingerprint persistence requested (`--add-fingerprint`) but \
                     deferred to slice 4B — cut applied to this book only; re-run \
                     after 4B lands to share the fingerprint across the library",
                );
            }
        }
    }
    Ok(())
}

/// `aborg audiologos import <path>` — one-shot `ABtagger`
/// fingerprint import.
///
/// Slice 4A reads + validates the JSON shape, prints a summary
/// of what *would* be imported, but does NOT yet insert into
/// `audiologos`. The actual insert lands once we know the
/// `ABtagger` export format (mapped via the user's tmp HTML
/// report). This is a deliberate stub — pre-flight the format
/// before committing to a writer.
async fn audiologo_import(path: &std::path::Path, _output: OutputFormat) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("import path does not exist: {}", path.display());
    }
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let _peek: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse JSON: {}", path.display()))?;
    tracing::warn!(
        path = ?path,
        size = bytes.len(),
        "audiologo.import: parser stub — schema mapping defers to slice 4B \
         (the ABtagger export shape needs to be confirmed against \
         the tmp HTML review report first). No rows inserted.",
    );
    Ok(())
}
