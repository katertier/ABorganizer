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

use std::path::{Path, PathBuf};
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
    /// Cross-library reports (BACKLOG § B). Subcommands `gaps` and
    /// `upcoming`.
    Report {
        #[command(subcommand)]
        action: ReportAction,
    },
    /// Cleanup subsystem (ADR-0025). Categories: disk, db, queue.
    ///
    /// Dry-run by default per ADR-0029 § "Mutating commands
    /// default to dry-run". `--commit` is the single opt-out
    /// flag that switches to delete mode; `--force` is the
    /// second tier that bypasses per-target age gating, and
    /// requires `--commit` to actually delete.
    Clean {
        /// One of `disk`, `db`, `queue`.
        category: String,
        /// Actually delete (default = dry-run). Per ADR-0029,
        /// this is the canonical "yes, mutate" flag across
        /// every mutating `aborg` command. The legacy `--apply`
        /// spelling was renamed to `--commit` in slice #87.
        #[arg(long)]
        commit: bool,
        /// Skip per-target age gating (per-target docs spell out
        /// what this means; pairing codes: invalidates every
        /// unconsumed code regardless of `expires_at`). Per
        /// ADR-0029 § "second tier", `--force` only relaxes
        /// safety checks; combine with `--commit` to actually
        /// delete.
        #[arg(long)]
        force: bool,
    },
    /// Show daemon health.
    Health,
    /// Print local CLI version and the running daemon's version
    /// side-by-side. Useful diagnostic when the operator just
    /// upgraded the CLI binary but the daemon is still running an
    /// older release — version drift between the two surfaces
    /// breaks subtle assumptions about which features the daemon
    /// supports. `--version` (clap auto-generated) still prints
    /// only the local CLI's compile-time semver; this subcommand
    /// is the one that crosses the wire.
    Version,
    /// Audible AAX inspector (read-only).
    ///
    /// AAX files are MP4 containers with encrypted audio samples
    /// but **unencrypted** metadata atoms. `aborg aax info` reads
    /// the codec tag + tags + duration without needing the
    /// operator's account activation bytes — useful for verifying
    /// a file *is* an AAX (`codec_tag = aavd`) before registering
    /// the bytes that unlock decrypt.
    Aax {
        #[command(subcommand)]
        action: AaxAction,
    },
}

#[derive(Debug, Subcommand)]
enum AaxAction {
    /// Print tags + codec tag + duration for an AAX file.
    Info {
        /// Path to the `.aax` or `.aaxc` file.
        path: PathBuf,
    },
    /// Store the operator's AAX activation bytes in the macOS
    /// Keychain (ADR-0053 path 3). Interactive prompt with
    /// hidden input — the value is never echoed, never logged,
    /// never written to a config file. The stored form is the
    /// lowercase 8-char hex string. To check whether bytes are
    /// configured (without revealing them), run `aborg doctor
    /// aax`.
    SetBytes,
    /// Remove the activation-bytes Keychain entry. Idempotent:
    /// no-op when nothing is stored. The env-var
    /// `ABORG_AAX_ACTIVATION_BYTES` and
    /// `Tunables.audio.aax_activation_bytes` paths are
    /// untouched — the operator manages those directly.
    ForgetBytes,
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
    /// List unresolved disambiguation rows. ADR-0026 H.3.6.
    Pending,
    /// Resolve a pending disambiguation. Either pick one of the
    /// candidates by id, or create a new identity row.
    ///
    /// Examples:
    ///
    ///   aborg names resolve 7 --kind author --pick 12
    ///   aborg names resolve 7 --kind author --create-new "Real Name" --audible-id B0X
    Resolve {
        /// Pending row id from `aborg names pending`.
        pending_id: i64,
        /// `author`, `narrator`, or `series`.
        #[arg(long)]
        kind: String,
        /// Existing identity row to pick. Mutually exclusive with
        /// `--create-new`.
        #[arg(long, conflicts_with = "create_new")]
        pick: Option<i64>,
        /// Canonical name for a brand-new identity row.
        #[arg(long, conflicts_with = "pick", requires = "kind")]
        create_new: Option<String>,
        /// Optional `audible_id` for the new identity row.
        #[arg(long, requires = "create_new")]
        audible_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ReportAction {
    /// Books Audible thinks <author> wrote but we don't have.
    /// BACKLOG § Cluster 5 / slice 10E.
    Gaps {
        /// `author_id` from `aborg book list` or `aborg names
        /// pending`.
        #[arg(long)]
        author: i64,
        /// Cap on Audible result pages (50/page). Default 5.
        #[arg(long)]
        max_pages: Option<u32>,
    },
    /// Upcoming releases across library authors. Use `--days N`
    /// to set the lookahead window. BACKLOG § Cluster 5 / slice
    /// 10F.
    Upcoming {
        /// Days into the future to look. Default 180; capped at 730.
        #[arg(long)]
        days: Option<u32>,
        /// Restrict to one author by id.
        #[arg(long)]
        author: Option<i64>,
        /// Cap on Audible result pages per author. Default 3.
        #[arg(long)]
        max_pages: Option<u32>,
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
    /// List `book_file_audiologos` rows at `status='candidate'`
    /// for operator review. Slice 4D.
    Review {
        /// Output format.
        #[arg(short = 'o', long, default_value = "human")]
        output: OutputFormat,
    },
    /// Approve a candidate audiologo row — promotes it to
    /// `status='applied'`, runs the chapter shift, and may
    /// auto-confirm the underlying `audiologos` row when its
    /// `match_count` has crossed the auto-confirm threshold.
    Approve {
        /// `book_file_audiologos.audiologo_row_id` of the
        /// candidate row to approve.
        row_id: i64,
        /// Optional operator note (currently logged only).
        #[arg(long)]
        note: Option<String>,
    },
    /// Reject a candidate audiologo row — flips it to
    /// `status='rejected'`. The chapter offsets are unchanged
    /// (no apply happened). Applied rows can't be rejected this
    /// way yet; the inverse chapter-shift maths is a future
    /// slice.
    Reject {
        /// `book_file_audiologos.audiologo_row_id` of the
        /// candidate row to reject.
        row_id: i64,
        /// Optional operator note (currently logged only).
        #[arg(long)]
        note: Option<String>,
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
    /// Show details of one book — core row, files, per-stage
    /// progress, audiologo status, chapter coverage. Diagnostic
    /// surface for "why didn't this book extract X?" questions.
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
    ///   aborg book retry 42 --stage read-tags
    ///   aborg book retry 42 --stage read-tags,fingerprint
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
    /// Apply a user-edit to one or more fields of a book.
    ///
    /// Thin shim over `PATCH /api/v1/books/{book_id}` (slice #89,
    /// ADR-0028 user-edit rule). Every flag is optional —
    /// supply at least one or the daemon returns 400. Edits
    /// land as `source='user_edit'` + `confidence=1.0` rows in
    /// `book_field_provenance` and stay sticky through the late
    /// tag-write pass.
    ///
    /// Join-driven fields (`author`, `narrator`, `publisher`,
    /// `series`, `genre`, `cover_url`) defer to a follow-up
    /// slice — they need resolve-identity plumbing on the
    /// server. v1 covers the scalar fields.
    ///
    /// Examples:
    ///
    ///   aborg book patch 42 --title "Corrected Title"
    ///   aborg book patch 42 --language en --explicit true
    ///   aborg book patch 42 --description "Fixed plot summary"
    Patch {
        /// Book ID — matches `books.book_id`.
        book_id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        subtitle: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// BCP-47 (e.g. `en`, `de`).
        #[arg(long)]
        language: Option<String>,
        /// ISO 8601 date string (`YYYY-MM-DD`).
        #[arg(long)]
        release_date: Option<String>,
        #[arg(long)]
        asin: Option<String>,
        #[arg(long)]
        isbn: Option<String>,
        /// Pass `true` or `false`. Omit to leave the field
        /// untouched.
        #[arg(long, value_name = "true|false")]
        abridged: Option<bool>,
        /// Pass `true` or `false`. Omit to leave the field
        /// untouched.
        #[arg(long, value_name = "true|false")]
        explicit: Option<bool>,
    },
    /// Hard-delete a book from the library DB.
    ///
    /// Per ADR-0029's two-tier rule for irreversible operations:
    /// `--commit` is the verb-shaped opt-in to mutate; `--force`
    /// is the second-tier explicit acknowledgement that this is
    /// the unrecoverable variant. Both flags are required.
    ///
    /// Cascade-deletes every per-book FK row
    /// (`book_field_provenance`, `book_files`, `chapters`, …).
    /// `mass_edit_history` audit rows survive with orphaned
    /// `target_id`. On-disk files in `book_files.file_path` are
    /// NOT removed by this command — reclaim disk space
    /// separately via `rm` (or a future cleanup target).
    ///
    /// v1 ONLY supports the hard-delete path. Soft-delete (the
    /// future default per API.md) ships when its schema +
    /// every-read-query filter lands.
    Delete {
        /// Book ID — matches `books.book_id`.
        book_id: i64,
        /// Required by ADR-0029. Without `--commit` the CLI
        /// refuses up front rather than round-trip and let the
        /// daemon reject — same dry-run-default safety net the
        /// rest of `aborg` follows.
        #[arg(long)]
        commit: bool,
        /// Required by ADR-0029 § "second tier" for irreversible
        /// operations. Maps to the daemon's `?force=true`.
        #[arg(long)]
        force: bool,
    },
    /// Un-soft-delete a book. Idempotent — restoring an
    /// already-active book is a no-op (no error). Slice #103.
    Restore {
        /// Book ID — matches `books.book_id`.
        book_id: i64,
        /// Required by ADR-0029. Restore is a mutating
        /// operation; without `--commit` the CLI refuses.
        /// No `--force` tier — restore is the gentle undo
        /// of a soft-delete; nothing irreversible to gate.
        #[arg(long)]
        commit: bool,
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
    /// Report whether AAX activation bytes are configured
    /// (env, config.toml, or Keychain). Never reveals the
    /// stored value — only the source tag.
    Aax,
    /// Print the resolved tunables tree (built-in defaults +
    /// `<storage_root>/config.toml` + `AB_*` env vars), with
    /// secret-material fields replaced by `<redacted>`. Useful
    /// for verifying that an operator-edited config.toml or
    /// `AB_FOO__BAR` env var is actually being picked up.
    Tunables,
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
// `main` is the top-level subcommand dispatch. Fragmenting the
// match obscures the verb table more than it helps; the
// per-handler bodies already live in separate functions. CLI
// surface grows roughly one variant per slice; cap exemption is
// justified by structure.
#[allow(clippy::too_many_lines)]
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
            BookAction::Show { id } => book_show(&cli.daemon, id, cli.output).await?,
            BookAction::Retry { book_id, stage } => {
                book_retry(&cli.daemon, book_id, &stage, cli.output).await?;
            }
            BookAction::Patch {
                book_id,
                title,
                subtitle,
                description,
                language,
                release_date,
                asin,
                isbn,
                abridged,
                explicit,
            } => {
                book_patch(
                    &cli.daemon,
                    book_id,
                    BookPatchArgs {
                        title,
                        subtitle,
                        description,
                        language,
                        release_date,
                        asin,
                        isbn,
                        abridged,
                        explicit,
                    },
                    cli.output,
                )
                .await?;
            }
            BookAction::Delete {
                book_id,
                commit,
                force,
            } => {
                book_delete(&cli.daemon, book_id, commit, force, cli.output).await?;
            }
            BookAction::Restore { book_id, commit } => {
                book_restore(&cli.daemon, book_id, commit, cli.output).await?;
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
            Some(DoctorAction::Aax) => doctor_aax(cli.output)?,
            Some(DoctorAction::Tunables) => doctor_tunables(cli.output)?,
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
            AudiologoAction::Review { output } => {
                audiologos_review(&cli.daemon, output).await?;
            }
            AudiologoAction::Approve { row_id, note } => {
                audiologos_approve(&cli.daemon, row_id, note.as_deref(), cli.output).await?;
            }
            AudiologoAction::Reject { row_id, note } => {
                audiologos_reject(&cli.daemon, row_id, note.as_deref(), cli.output).await?;
            }
        },
        Command::Names { action } => match action {
            NamesAction::Alias { id, kind, add } => {
                names_alias(&cli.daemon, id, &kind, &add, cli.output).await?;
            }
            NamesAction::Exalt { id, kind, alias } => {
                names_exalt(&cli.daemon, id, &kind, &alias, cli.output).await?;
            }
            NamesAction::Pending => {
                names_pending(&cli.daemon, cli.output).await?;
            }
            NamesAction::Resolve {
                pending_id,
                kind,
                pick,
                create_new,
                audible_id,
            } => {
                names_resolve(
                    &cli.daemon,
                    pending_id,
                    &kind,
                    pick,
                    create_new.as_deref(),
                    audible_id.as_deref(),
                    cli.output,
                )
                .await?;
            }
        },
        Command::Report { action } => match action {
            ReportAction::Gaps { author, max_pages } => {
                report_gaps(&cli.daemon, author, max_pages, cli.output).await?;
            }
            ReportAction::Upcoming {
                days,
                author,
                max_pages,
            } => {
                report_upcoming(&cli.daemon, days, author, max_pages, cli.output).await?;
            }
        },
        Command::Clean {
            category,
            commit,
            force,
        } => {
            clean(&cli.daemon, &category, commit, force, cli.output).await?;
        }
        Command::Health => health(&cli.daemon, cli.output).await?,
        Command::Version => version_cmd(&cli.daemon, cli.output).await?,
        Command::Aax { action } => match action {
            AaxAction::Info { path } => aax_info(&path, cli.output)?,
            AaxAction::SetBytes => aax_set_bytes(cli.output)?,
            AaxAction::ForgetBytes => aax_forget_bytes(cli.output)?,
        },
    }
    Ok(())
}

// ── AAX inspector ────────────────────────────────────────────────────

fn aax_info(path: &Path, output: OutputFormat) -> Result<()> {
    let info = ab_audio::read_aax_info(path)
        .with_context(|| format!("read AAX info from {}", path.display()))?;
    match output {
        OutputFormat::Json => {
            let payload = serde_json::json!({
                "path": path.display().to_string(),
                "codec_tag": info.codec_tag,
                "is_aax": info.is_aax,
                "duration_ms": info.duration_ms,
                "title": info.title,
                "author": info.author,
                "narrator": info.narrator,
                "album": info.album,
                "genre": info.genre,
                "description": info.description,
                "copyright": info.copyright,
                "chapter_count": info.chapter_count,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            println!("path:          {}", path.display());
            println!(
                "codec_tag:     {} ({})",
                info.codec_tag.as_deref().unwrap_or("?"),
                if info.is_aax {
                    "Audible AAX — decrypt needed"
                } else {
                    "not AAX"
                },
            );
            if let Some(ms) = info.duration_ms {
                let secs = ms / 1000;
                println!(
                    "duration:      {ms} ms  ({}h {}m)",
                    secs / 3600,
                    (secs / 60) % 60
                );
            }
            print_field("title", info.title.as_deref());
            print_field("author", info.author.as_deref());
            print_field("narrator", info.narrator.as_deref());
            print_field("album", info.album.as_deref());
            print_field("genre", info.genre.as_deref());
            print_field("copyright", info.copyright.as_deref());
            print_field("description", info.description.as_deref());
            println!("chapter_count: {}", info.chapter_count);
        }
    }
    Ok(())
}

fn print_field(label: &str, value: Option<&str>) {
    if let Some(v) = value {
        println!("{label:<14} {v}");
    }
}

// ── AAX activation-bytes CLI ─────────────────────────────────────────

fn aax_set_bytes(output: OutputFormat) -> Result<()> {
    let raw = rpassword::prompt_password("AAX activation bytes (8 hex chars, input hidden): ")
        .context("read activation-bytes prompt")?;
    let bytes = ab_core::aax_activation_bytes::ActivationBytes::parse(&raw)
        .map_err(|e| anyhow::anyhow!("invalid activation bytes: {e}"))?;
    ab_core::aax_activation_bytes::keychain::set(&bytes)
        .context("store activation bytes in keychain")?;
    match output {
        OutputFormat::Json => {
            let payload = serde_json::json!({"stored": true, "source": "keychain"});
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            println!("✓ stored in keychain");
            println!("   run `aborg doctor aax` to verify.");
        }
    }
    Ok(())
}

fn aax_forget_bytes(output: OutputFormat) -> Result<()> {
    ab_core::aax_activation_bytes::keychain::forget()
        .context("remove activation bytes from keychain")?;
    match output {
        OutputFormat::Json => {
            let payload = serde_json::json!({"removed": true});
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            println!("✓ keychain entry removed (idempotent — no error if absent).");
        }
    }
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn doctor_tunables(output: OutputFormat) -> Result<()> {
    let storage_root = ab_core::paths::app_support_dir();
    let tunables = ab_core::Tunables::load(&storage_root).context("load tunables")?;
    let mut value = serde_json::to_value(&tunables).context("serialise tunables")?;
    redact_tunables_secrets(&mut value);

    let pretty = serde_json::to_string_pretty(&value).unwrap_or_default();
    match output {
        OutputFormat::Json => {
            println!("{pretty}");
        }
        OutputFormat::Human => {
            println!("# Tunables (resolved: defaults → config.toml → AB_* env)");
            println!("# Secrets redacted. JSON shape.");
            println!("{pretty}");
        }
    }
    Ok(())
}

/// Mutate `value` in place, replacing known-secret fields with
/// `"<redacted>"`. Keeps the JSON shape so the operator can still
/// tell whether a value is set (the redaction is visible) without
/// revealing the underlying bytes.
///
/// Today's secret-field list (kept tiny on purpose — when this
/// grows past 4-5 entries, hoist to a method on Tunables that
/// owns the list at the type level):
///
/// * `audio.aax_activation_bytes` (ADR-0053)
fn redact_tunables_secrets(value: &mut serde_json::Value) {
    use serde_json::Value;
    if let Value::Object(map) = value {
        if let Some(Value::Object(audio)) = map.get_mut("audio") {
            if let Some(slot) = audio.get_mut("aax_activation_bytes") {
                if !slot.is_null() {
                    *slot = Value::String("<redacted>".to_owned());
                }
            }
        }
    }
}

#[allow(clippy::unnecessary_wraps)]
fn doctor_aax(output: OutputFormat) -> Result<()> {
    let tunables = ab_core::tunables::Tunables::default();
    let resolved = ab_core::aax_activation_bytes::resolve(&tunables.audio);
    match output {
        OutputFormat::Json => {
            let payload = match &resolved {
                Some((_, source)) => serde_json::json!({
                    "configured": true,
                    "source": source,
                }),
                None => serde_json::json!({
                    "configured": false,
                    "source": serde_json::Value::Null,
                }),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            println!("audiobook AAX decrypt:");
            match resolved {
                Some((_, source)) => {
                    println!("  activation bytes  ✓ configured (via {})", source.tag());
                }
                None => {
                    println!("  activation bytes  ✗ not configured  (see `aborg aax set-bytes`)");
                }
            }
        }
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

async fn library_scan(daemon: &str, path: &Path, output: OutputFormat) -> Result<()> {
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
            // Clean stdout JSON — `| jq` pipeline support.
            println!(
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
    /// Total matching rows on the daemon side, NOT clamped by the
    /// page-`limit`. `#[serde(default)]` keeps the CLI parsing
    /// older daemons that haven't shipped the field yet (they
    /// surface as `total = 0`; `aborg version` flags the drift
    /// before the operator hits any meaningful divergence).
    #[serde(default)]
    total: i64,
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
    // (`--stage read-tags,all`) since that's ambiguous.
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
            // Clean stdout JSON so `--output json | jq` works.
            // `tracing::info!` would wrap this in a tracing-formatter
            // prefix and break the pipeline.
            println!(
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

// ── `aborg book show <id>` ────────────────────────────────────────

#[derive(Deserialize, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct BookFileDetail {
    file_id: i64,
    file_path: String,
    duration_ms: Option<i64>,
    file_size: Option<i64>,
    is_active: bool,
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct StageProgressRow {
    stage: String,
    status: String,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    failure_reason: Option<String>,
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct ChapterSourceCount {
    source: String,
    count: i64,
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "JSON response struct mirroring the API contract — each bool is an independent presence flag for a specific AI-derived field; a state machine doesn't fit the open-set semantics."
)]
struct BookDetailResponse {
    book_id: i64,
    title: String,
    subtitle: Option<String>,
    description: Option<String>,
    language: Option<String>,
    duration_ms: Option<i64>,
    asin: Option<String>,
    isbn: Option<String>,
    release_date: Option<String>,
    abridged: Option<bool>,
    explicit: Option<bool>,
    audiologo_status: String,
    author: Option<String>,
    narrators: Option<String>,
    publisher: Option<String>,
    series: Option<String>,
    has_summary: bool,
    has_story_arc: bool,
    has_setting: bool,
    has_characters: bool,
    files: Vec<BookFileDetail>,
    stages: Vec<StageProgressRow>,
    chapters: Vec<ChapterSourceCount>,
}

/// `aborg book show <id> [--output human|json]` — thin shim over
/// `GET /api/v1/books/{book_id}`. Diagnostic surface; mirrors the
/// shape of the JSON response when `--output json`.
async fn book_show(daemon: &str, id: i64, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/books/{id}");
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("book show failed: HTTP {status}: {body}");
    }
    let body: BookDetailResponse = resp.json().await.context("parse book detail")?;

    match output {
        OutputFormat::Json => {
            // Clean stdout JSON so `--output json | jq` works.
            // `tracing::info!` would wrap this in a tracing-formatter
            // prefix and break the pipeline.
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            let author = body.author.as_deref().unwrap_or("");
            let narrators = body.narrators.as_deref().unwrap_or("");
            let publisher = body.publisher.as_deref().unwrap_or("");
            let series = body.series.as_deref().unwrap_or("");
            let language = body.language.as_deref().unwrap_or("");
            let asin = body.asin.as_deref().unwrap_or("");
            let isbn = body.isbn.as_deref().unwrap_or("");
            let release = body.release_date.as_deref().unwrap_or("");
            let subtitle = body.subtitle.as_deref().unwrap_or("");
            let duration_min = body.duration_ms.map_or(0, |ms| ms / 60_000);
            tracing::info!(
                book_id = body.book_id,
                title = %body.title,
                subtitle = %subtitle,
                author = %author,
                narrators = %narrators,
                publisher = %publisher,
                series = %series,
                language = %language,
                asin = %asin,
                isbn = %isbn,
                release_date = %release,
                duration_min,
                abridged = body.abridged.unwrap_or(false),
                explicit = body.explicit.unwrap_or(false),
                audiologo_status = %body.audiologo_status,
                has_summary = body.has_summary,
                has_story_arc = body.has_story_arc,
                has_setting = body.has_setting,
                has_characters = body.has_characters,
                "book.detail"
            );
            for f in &body.files {
                tracing::info!(
                    file_id = f.file_id,
                    path = %f.file_path,
                    duration_ms = f.duration_ms.unwrap_or(0),
                    file_size = f.file_size.unwrap_or(0),
                    is_active = f.is_active,
                    "book.file"
                );
            }
            for s in &body.stages {
                let reason = s.failure_reason.as_deref().unwrap_or("");
                tracing::info!(
                    stage = %s.stage,
                    status = %s.status,
                    started_at = s.started_at.unwrap_or(0),
                    completed_at = s.completed_at.unwrap_or(0),
                    failure_reason = %reason,
                    "book.stage"
                );
            }
            for c in &body.chapters {
                tracing::info!(
                    source = %c.source,
                    count = c.count,
                    "book.chapters"
                );
            }
        }
    }
    Ok(())
}

// ── `aborg book patch <id> …` ────────────────────────────────────

/// Per-field arguments threaded into [`book_patch`]. Packed into
/// one struct so the dispatcher arm doesn't need a 10-arg call.
struct BookPatchArgs {
    title: Option<String>,
    subtitle: Option<String>,
    description: Option<String>,
    language: Option<String>,
    release_date: Option<String>,
    asin: Option<String>,
    isbn: Option<String>,
    abridged: Option<bool>,
    explicit: Option<bool>,
}

#[derive(Serialize, Debug)]
struct BookPatchRequest<'a> {
    // `skip_serializing_if` keeps the wire body tidy (no `null`
    // soup when most flags are absent). The server's
    // deserializer treats absent and `null` the same way for
    // `Option<T>`, so this is wire-shape polish, not semantics.
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_date: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    asin: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    isbn: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    abridged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    explicit: Option<bool>,
}

#[derive(Deserialize, Debug, Serialize)]
struct BookPatchResponse {
    book_id: i64,
    updated: Vec<String>,
}

/// `aborg book patch <book_id> [field-flags]` — thin shim over
/// `PATCH /api/v1/books/{book_id}` (slice #89). Empty patch is
/// rejected server-side with 400; surface that message directly
/// rather than pre-validating client-side.
async fn book_patch(
    daemon: &str,
    book_id: i64,
    args: BookPatchArgs,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/books/{book_id}");
    let body = BookPatchRequest {
        title: args.title.as_deref(),
        subtitle: args.subtitle.as_deref(),
        description: args.description.as_deref(),
        language: args.language.as_deref(),
        release_date: args.release_date.as_deref(),
        asin: args.asin.as_deref(),
        isbn: args.isbn.as_deref(),
        abridged: args.abridged,
        explicit: args.explicit,
    };
    let resp = client()
        .patch(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("PATCH {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("book patch failed: HTTP {status}: {body}");
    }
    let body: BookPatchResponse = resp.json().await.context("parse patch response")?;
    match output {
        OutputFormat::Json => {
            // Clean stdout JSON — `| jq` pipeline support.
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            if body.updated.is_empty() {
                tracing::info!(book_id = body.book_id, "book.patched (no fields updated)");
            } else {
                let fields = body.updated.join(", ");
                tracing::info!(
                    book_id = body.book_id,
                    updated = %fields,
                    "book.patched"
                );
            }
        }
    }
    Ok(())
}

// ── `aborg book delete <id> --commit [--force]` ──────────────────

/// `aborg book delete <book_id> --commit [--force]` — soft- or
/// hard-delete a book over `DELETE /api/v1/books/{book_id}`.
///
/// Per ADR-0029:
/// - `--commit` (alone) → **soft-delete** (reversible; row stays
///   in the DB with `deleted_at` set; future restore endpoint
///   can flip it back).
/// - `--commit --force` → **hard-delete** (irreversible CASCADE).
///
/// `--force` alone is refused — mutation always requires
/// `--commit` (the dry-run-default safety net).
async fn book_delete(
    daemon: &str,
    book_id: i64,
    commit: bool,
    force: bool,
    output: OutputFormat,
) -> Result<()> {
    if !commit {
        anyhow::bail!(
            "refusing to delete without --commit (per ADR-0029 dry-run-default). \
             `--commit` alone soft-deletes; add `--force` for an irreversible \
             hard delete."
        );
    }
    // `?force=true` only when the CLI was given `--force` too.
    // Soft-delete is the default at both layers.
    let url = if force {
        format!("{daemon}/api/v1/books/{book_id}?force=true")
    } else {
        format!("{daemon}/api/v1/books/{book_id}")
    };
    let resp = client()
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("book delete failed: HTTP {status}: {body}");
    }
    let mode = if force { "hard" } else { "soft" };
    match output {
        OutputFormat::Json => {
            // 204 NoContent — emit a structured success body so
            // operators have something to pipe.
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "book_id": book_id,
                    "deleted": true,
                    "mode": mode,
                }))
                .unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            tracing::info!(book_id, mode, "book.deleted");
        }
    }
    Ok(())
}

// ── `aborg book restore <id> --commit` ───────────────────────────

/// `aborg book restore <book_id> --commit` — un-soft-delete a
/// book over `POST /api/v1/books/{book_id}/restore`.
///
/// Refuses without `--commit` (mutating operation; matches the
/// dry-run-default convention). No `--force` tier — restore is
/// the gentle undo of a soft-delete; nothing irreversible to
/// gate.
async fn book_restore(
    daemon: &str,
    book_id: i64,
    commit: bool,
    output: OutputFormat,
) -> Result<()> {
    if !commit {
        anyhow::bail!(
            "refusing to restore without --commit (per ADR-0029 dry-run-default). \
             `aborg book restore <id> --commit` is the full incantation."
        );
    }
    let url = format!("{daemon}/api/v1/books/{book_id}/restore");
    let resp = client()
        .post(&url)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("book restore failed: HTTP {status}: {body}");
    }
    // 200 with body { book_id, restored: bool }
    let parsed: serde_json::Value = resp.json().await.context("parse restore response")?;
    let restored = parsed
        .get("restored")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    match output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&parsed).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            if restored {
                tracing::info!(book_id, "book.restored");
            } else {
                tracing::info!(book_id, "book.restore_noop (book was already active)");
            }
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
            // Clean stdout JSON so `--output json | jq` works.
            // `tracing::info!` would wrap this in a tracing-formatter
            // prefix and break the pipeline.
            println!(
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
                tracing::info!(
                    shown = body.books.len(),
                    total = body.total,
                    "books.list.done",
                );
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
            // Clean stdout JSON so `--output json | jq` works.
            // `tracing::info!` would wrap this in a tracing-formatter
            // prefix and break the pipeline.
            println!(
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
struct DaemonVersionResponse {
    name: String,
    version: String,
    description: String,
}

/// Combined local + remote version readout. Both fields land in the
/// JSON branch; the human branch tags drift inline ("⚠ version
/// mismatch") so the operator sees the issue without parsing two
/// numbers.
#[derive(Serialize)]
struct VersionPair {
    cli_version: &'static str,
    daemon: DaemonVersionResponse,
}

async fn version_cmd(daemon: &str, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/version");
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("version fetch failed: HTTP {status}");
    }
    let body: DaemonVersionResponse = resp.json().await.context("parse version response")?;
    let cli_version: &'static str = ab_core::build_info::VERSION;
    let drift = cli_version != body.version;
    match output {
        OutputFormat::Json => {
            let pair = VersionPair {
                cli_version,
                daemon: body,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&pair).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            if drift {
                tracing::warn!(
                    cli = %cli_version,
                    daemon_app = %body.name,
                    daemon_version = %body.version,
                    "⚠ version drift: CLI {cli_version} vs daemon {dv}",
                    dv = body.version,
                );
            } else {
                tracing::info!(
                    cli = %cli_version,
                    daemon_app = %body.name,
                    daemon_version = %body.version,
                    "CLI and daemon both at {cli_version}",
                );
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
            // Clean stdout JSON so `--output json | jq` works.
            // `tracing::info!` would wrap this in a tracing-formatter
            // prefix and break the pipeline.
            println!(
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
    commit: bool,
    force: bool,
    age_seconds: i64,
    targets: Vec<CleanReportRow>,
}

#[derive(Serialize, Debug)]
struct CleanRunRequest<'a> {
    category: &'a str,
    commit: bool,
    force: bool,
}

/// `aborg clean <category> [--commit] [--force]` — thin shim over
/// `POST /api/v1/clean/run`. Dry-run by default per ADR-0029;
/// `--commit` deletes; `--force` ignores per-target age gates.
/// ADR-0025.
async fn clean(
    daemon: &str,
    category: &str,
    commit: bool,
    force: bool,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/clean/run");
    let resp = client()
        .post(&url)
        .json(&CleanRunRequest {
            category,
            commit,
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
            let mode = if body.commit { "committed" } else { "dry-run" };
            let force_note = if body.force { " (forced)" } else { "" };
            let age_days = body.age_seconds / 86_400;
            println!(
                "clean {} {} {}{} — age cut-off {} d, {} target(s)",
                body.category,
                mode,
                if body.commit { "→" } else { "↦" },
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
            if !body.commit && (total_items > 0 || total_bytes > 0) {
                println!("\nrun with --commit to delete.");
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

#[derive(Deserialize, Debug, Serialize)]
struct PendingCandidate {
    id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audible_id: Option<String>,
    score: f64,
}

#[derive(Deserialize, Debug, Serialize)]
struct PendingRow {
    pending_id: i64,
    kind: String,
    book_id: i64,
    observed_alias: String,
    created_at: i64,
    candidates: Vec<PendingCandidate>,
}

async fn names_pending(daemon: &str, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/names/pending");
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("pending list failed: HTTP {status}: {body}");
    }
    let rows: Vec<PendingRow> = resp.json().await.context("parse pending list")?;
    match output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&rows).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            if rows.is_empty() {
                println!("no pending disambiguations");
            } else {
                for r in &rows {
                    println!(
                        "pending #{} — {} book {} — observed `{}`",
                        r.pending_id, r.kind, r.book_id, r.observed_alias
                    );
                    for c in &r.candidates {
                        let display = c.display.as_deref().unwrap_or("?");
                        let asin = c.audible_id.as_deref().unwrap_or("");
                        println!(
                            "  id {:>4}  score {:>4.2}  {}  {}",
                            c.id, c.score, display, asin
                        );
                    }
                }
                println!("{} pending row(s)", rows.len());
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct ResolveRequest<'a> {
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pick: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    create_new: Option<ResolveCreateNew<'a>>,
}

#[derive(Serialize)]
struct ResolveCreateNew<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    audible_id: Option<&'a str>,
}

#[derive(Deserialize, Debug, Serialize)]
struct ResolveResponse {
    pending_id: i64,
    kind: String,
    book_id: i64,
    resolved_id: i64,
}

#[allow(clippy::too_many_arguments)] // CLI dispatch glue; bundling adds boilerplate without semantic gain
async fn names_resolve(
    daemon: &str,
    pending_id: i64,
    kind: &str,
    pick: Option<i64>,
    create_new: Option<&str>,
    audible_id: Option<&str>,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/names/pending/{pending_id}/resolve");
    let body = ResolveRequest {
        kind,
        pick,
        create_new: create_new.map(|name| ResolveCreateNew { name, audible_id }),
    };
    let resp = client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        anyhow::bail!("resolve failed: HTTP {status}: {txt}");
    }
    let r: ResolveResponse = resp.json().await.context("parse resolve response")?;
    match output {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        }
        OutputFormat::Human => {
            println!(
                "resolved #{} — {} book {} → {} id {}",
                r.pending_id, r.kind, r.book_id, r.kind, r.resolved_id
            );
        }
    }
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

// ── Reports (slices 10E + 10F, Cluster 5) ────────────────────────────

#[derive(Deserialize, Debug, Serialize)]
struct BookCandidate {
    asin: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_min: Option<u32>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    narrators: Vec<String>,
}

#[derive(Deserialize, Debug, Serialize)]
struct GapsResponseBody {
    author_id: i64,
    author_name: String,
    owned_count: u64,
    gap_count: u64,
    books: Vec<BookCandidate>,
}

async fn report_gaps(
    daemon: &str,
    author: i64,
    max_pages: Option<u32>,
    output: OutputFormat,
) -> Result<()> {
    let url = max_pages.map_or_else(
        || format!("{daemon}/api/v1/report/gaps?author={author}"),
        |mp| format!("{daemon}/api/v1/report/gaps?author={author}&max_pages={mp}"),
    );
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("gaps failed: HTTP {status}: {body}");
    }
    let body: GapsResponseBody = resp.json().await.context("parse gaps response")?;
    match output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            println!(
                "author #{} — {} — {} owned / {} gap",
                body.author_id, body.author_name, body.owned_count, body.gap_count
            );
            for b in &body.books {
                let date = b.release_date.as_deref().unwrap_or("");
                println!("  [{:>8}] {} — {} ({})", b.status, b.asin, b.title, date);
            }
        }
    }
    Ok(())
}

#[derive(Deserialize, Debug, Serialize)]
struct UpcomingResponseBody {
    days_window: u32,
    authors_checked: u64,
    books: Vec<BookCandidate>,
}

async fn report_upcoming(
    daemon: &str,
    days: Option<u32>,
    author: Option<i64>,
    max_pages: Option<u32>,
    output: OutputFormat,
) -> Result<()> {
    let mut params: Vec<String> = Vec::new();
    if let Some(d) = days {
        params.push(format!("days={d}"));
    }
    if let Some(a) = author {
        params.push(format!("author={a}"));
    }
    if let Some(m) = max_pages {
        params.push(format!("max_pages={m}"));
    }
    let url = if params.is_empty() {
        format!("{daemon}/api/v1/upcoming")
    } else {
        format!("{daemon}/api/v1/upcoming?{}", params.join("&"))
    };
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("upcoming failed: HTTP {status}: {body}");
    }
    let body: UpcomingResponseBody = resp.json().await.context("parse upcoming response")?;
    match output {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            println!(
                "{} day window — {} authors checked — {} upcoming",
                body.days_window,
                body.authors_checked,
                body.books.len()
            );
            for b in &body.books {
                let date = b.release_date.as_deref().unwrap_or("?");
                let authors = b.authors.join(", ");
                println!(
                    "  {} [{}]  {} — {}  ({})",
                    date, b.status, b.title, authors, b.asin
                );
            }
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
            // Clean stdout JSON — `| jq` pipeline support.
            println!(
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
async fn audiologo_import(path: &Path, _output: OutputFormat) -> Result<()> {
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

// ── Audiologos review (slice 4D) ─────────────────────────────────

/// One row from `GET /api/v1/audiologos/review`. Mirrors
/// `ab_api::audiologo_review::ReviewRow`.
#[derive(serde::Deserialize, serde::Serialize)]
struct ReviewRow {
    row_id: i64,
    book_id: i64,
    book_title: String,
    file_id: i64,
    file_path: String,
    kind: String,
    jingle_start_ms: i64,
    jingle_end_ms: i64,
    method: String,
    confidence: f64,
    audiologo_id: Option<i64>,
    audiologo_name: Option<String>,
}

#[derive(serde::Serialize)]
struct ReviewActionRequest<'a> {
    note: Option<&'a str>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ApproveResponse {
    row_id: i64,
    applied_row_id: i64,
    chapters_shifted: i64,
    auto_confirmed: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct RejectResponse {
    row_id: i64,
}

async fn audiologos_review(daemon: &str, output: OutputFormat) -> Result<()> {
    let url = format!("{daemon}/api/v1/audiologos/review");
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("review failed: HTTP {status}: {body}");
    }
    let rows: Vec<ReviewRow> = resp.json().await.context("parse review response")?;

    match output {
        OutputFormat::Json => {
            // Clean stdout JSON — `| jq` pipeline support.
            let s = serde_json::to_string_pretty(&rows).context("encode json")?;
            println!("{s}");
        }
        OutputFormat::Human => {
            if rows.is_empty() {
                tracing::info!("audiologos.review.empty");
                return Ok(());
            }
            for r in &rows {
                let logo = r.audiologo_name.as_deref().unwrap_or("(no fingerprint)");
                tracing::info!(
                    row_id = r.row_id,
                    book_id = r.book_id,
                    title = %r.book_title,
                    kind = %r.kind,
                    method = %r.method,
                    confidence = r.confidence,
                    jingle_ms = format!("[{}, {}]", r.jingle_start_ms, r.jingle_end_ms),
                    audiologo = %logo,
                    "audiologos.review.row"
                );
            }
            tracing::info!(total = rows.len(), "audiologos.review.summary");
        }
    }
    Ok(())
}

async fn audiologos_approve(
    daemon: &str,
    row_id: i64,
    note: Option<&str>,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/audiologos/{row_id}/approve");
    let resp = client()
        .post(&url)
        .json(&ReviewActionRequest { note })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("approve failed: HTTP {status}: {body}");
    }
    let body: ApproveResponse = resp.json().await.context("parse approve response")?;
    match output {
        OutputFormat::Json => {
            // Clean stdout JSON — `| jq` pipeline support.
            let s = serde_json::to_string_pretty(&body).context("encode")?;
            println!("{s}");
        }
        OutputFormat::Human => {
            tracing::info!(
                row_id = body.row_id,
                applied_row_id = body.applied_row_id,
                chapters_shifted = body.chapters_shifted,
                auto_confirmed = body.auto_confirmed,
                "audiologos.approve.done"
            );
        }
    }
    Ok(())
}

async fn audiologos_reject(
    daemon: &str,
    row_id: i64,
    note: Option<&str>,
    output: OutputFormat,
) -> Result<()> {
    let url = format!("{daemon}/api/v1/audiologos/{row_id}/reject");
    let resp = client()
        .post(&url)
        .json(&ReviewActionRequest { note })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("reject failed: HTTP {status}: {body}");
    }
    let body: RejectResponse = resp.json().await.context("parse reject response")?;
    match output {
        OutputFormat::Json => {
            // Clean stdout JSON — `| jq` pipeline support.
            let s = serde_json::to_string_pretty(&body).context("encode")?;
            println!("{s}");
        }
        OutputFormat::Human => {
            tracing::info!(row_id = body.row_id, "audiologos.reject.done");
        }
    }
    Ok(())
}
