//! `aborg-daemon` — single-process daemon.
//!
//! Hosts (default features):
//!
//! * Native API at `/api/v1/` on the API port
//! * Player web UI at `/player/`
//! * Config web UI at `/config/`
//! * `Audiobookshelf`-compat API at root, on the ABS port
//! * Background pipeline workers (scan, fingerprint, enrich, ...)
//!
//! Cargo features (`tagger`, `player`, `shelf`) gate which routers
//! are registered. Default builds enable all three.

#![allow(
    missing_docs,
    // Internal modules use pub(crate) — clippy flags this as redundant
    // inside a binary crate, but it's the canonical visibility.
    clippy::redundant_pub_crate
)]

mod disk_usage;
mod lockfile;
mod logging;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::Router;
use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use std::sync::Arc;

use ab_core::{Tunables, paths};
use ab_db::{EphemeralDb, LibraryDb};
use ab_pipeline::cleanup::{
    CleanupCtx, CleanupLoopCtx, CleanupRegistry, CleanupTarget, run_cleanup_loop,
};
use ab_pipeline::{Dag, Scheduler, Stage, StageContext};

#[derive(Debug, Parser)]
#[command(
    name = ab_core::build_info::DAEMON_NAME,
    version = ab_core::build_info::VERSION,
    about = "ABorganizer daemon",
)]
struct Args {
    /// Run in foreground (default when no `LaunchAgent` is involved).
    #[arg(long)]
    foreground: bool,

    /// Path to config TOML (default: `<storage_root>/config.toml`).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
}

/// Build the per-book pipeline stage list. Lives here (not in a
/// library crate) because it's the daemon's wiring: it picks
/// concrete stage impls + threads each stage's tunables.
///
/// Declaration order = the order stages appear in the DAG's
/// `iter_topo()`. Actual run-time ordering is enforced by each
/// stage's `requires()` declaration; the order here is just for
/// human readability when reading the boot log.
fn build_pipeline_stages(tunables: &Tunables) -> Vec<Arc<dyn Stage>> {
    let audnexus_client = ab_catalog::AudnexusClient::new(&tunables.http_client);
    let audible_client = ab_catalog::AudibleClient::new(&tunables.http_client);
    let mut stages: Vec<Arc<dyn Stage>> = vec![
        // `tag-read` (slice 1B) — lofty MP4-atom / ID3 reader.
        // Writes title/author/subtitle/description/narrator
        // candidate rows; no dependencies.
        Arc::new(ab_tag_read::TagReadStage::new(tunables.tag_read.clone())),
        // `fingerprint` (slice 1C) — chromaprint whole-book hash.
        Arc::new(ab_fingerprint::FingerprintStage::new()),
        // `audible-search` fills in an ASIN candidate for books
        // with no `CatalogNumber` tag.
        Arc::new(ab_catalog::AudibleSearchStage::new(
            audible_client,
            &tunables.network,
        )),
        // `audnexus-enrich` waits for both tag-read AND
        // audible-search so it sees whichever ASIN source landed
        // first.
        Arc::new(ab_catalog::AudnexusEnrichStage::new(
            audnexus_client,
            &tunables.network,
        )),
        // `consensus` promotes the highest-confidence provenance
        // value into the corresponding `books` column.
        Arc::new(ab_catalog::ConsensusStage::new()),
        // `identity-resolve` promotes author / publisher / narrator
        // candidates into the identity tables + junctions, after
        // consensus has settled the scalar columns.
        Arc::new(ab_catalog::IdentityResolveStage::new()),
        // `audnexus-chapters` fetches the per-ASIN chapter ToC +
        // brand intro/outro markers.
        Arc::new(ab_catalog::AudnexusChaptersStage::new(
            ab_catalog::AudnexusClient::new(&tunables.http_client),
            &tunables.network,
        )),
        // `embedded-chapters` reads chpl + chapter-track atoms
        // from .m4b / .m4a files via mp4ameta.
        Arc::new(ab_catalog::EmbeddedChaptersStage::new()),
        // `chapter-pick-winner` flips `is_winner` so exactly one
        // chapter source per book is surfaced to the player.
        Arc::new(ab_catalog::ChapterWinnerStage::new()),
        // `transcribe-head-tail` (slice 3A.4) runs the on-device
        // Speech engine over the first 6 min + last 30 s of the
        // book, stores both transcripts in `ai_cache` keyed by
        // `extractor_version`, and seeds the language candidates
        // (pre- + post-transcribe).
        Arc::new(ab_transcript::TranscribeHeadTailStage::new(
            &tunables.transcribe,
            &tunables.language,
        )),
        // `detect-description-lang` (slice 3G) populates
        // `books.description_lang` once consensus picks the
        // description. Cheap pure-text NL detection; the UI
        // uses it for correct directionality / font rendering
        // when the description language differs from the
        // library locale.
        Arc::new(ab_transcript::DetectDescriptionLangStage::new(
            &tunables.language,
        )),
        // `transcribe-samples` (slice 3D.2) transcribes short
        // windows at 25/50/75% of the book at Background
        // priority. Provides the authoritative language signal
        // (deep enough to dodge jingles + non-native intros)
        // and a fast DNA-tag corpus before the full-book
        // transcribe completes.
        Arc::new(ab_transcript::TranscribeSamplesStage::new(
            &tunables.transcribe,
            &tunables.language,
        )),
        // `transcribe-full` (slice 3B) runs the whole book at
        // Idle priority — drains only when interactive + bg
        // queues are quiet. Locale is read from the head-
        // transcript cache. Chunked in Rust until the Swift
        // `AVAssetReader` rewrite lands.
        Arc::new(ab_transcript::TranscribeFullStage::new(
            &tunables.transcribe,
        )),
        // `run-transcript-extractors` (slice 3C) runs every
        // built-in heuristic extractor (title/author confirm,
        // tier-4 audiologo, ...) over the cached head transcript
        // and writes candidates to `book_field_provenance`.
        // Cheap — pure-text regex / keyword passes; no FFI.
        Arc::new(ab_transcript::RunExtractorsStage::new()),
        // `extract-dna-tags` (slice 3K.3, retrofitted to
        // complete_structured in C5.7.d) — Apple-Intelligence
        // pass over the full transcript producing `#`-prefixed
        // safe DNA tags + `!`-prefixed spoiler tags. Skips when
        // FoundationModels is unavailable; idempotent on
        // `extractor_version`.
        Arc::new(ab_llm_extractors::ExtractDnaTagsStage::new(&tunables.llm)),
        // `extract-summary-spoiler-free` (slice 3K.4) — Apple-
        // Intelligence pass producing a spoiler-free book summary
        // in `books.language`. Output stays in the book's native
        // language regardless of `library_locale` (per project
        // policy — library_locale is reserved for genre
        // vocabulary).
        Arc::new(ab_llm_extractors::ExtractSummaryStage::new(&tunables.llm)),
        // `extract-story-arc` (slice 3K.5) — Apple-Intelligence
        // pass producing a 5-7 beat narrative arc into
        // `books.story_arc_json`. Depends on transcribe-full
        // + extract-summary-spoiler-free (per ADR-0022, the
        // summary dependency sequences LLM calls per book).
        // Spoiler-gating happens at the read surface, not the
        // model.
        Arc::new(ab_llm_extractors::ExtractStoryArcStage::new(&tunables.llm)),
        // `extract-characters` (slice 3K.6) — Apple-Intelligence
        // pass producing up to 12 characters per book into the
        // `characters` table, with `is_pov` + 6 trait columns
        // (migration 008). Depends on transcribe-full + summary
        // per ADR-0022's per-book content extractor template.
        Arc::new(ab_llm_extractors::ExtractCharactersStage::new(
            &tunables.llm,
        )),
        // `extract-setting` (slice 3K.8) — Apple-Intelligence
        // pass producing a one-paragraph setting summary
        // (books.setting + _lang + _extractor_version,
        // migration 009) plus 10-category `$`-prefixed tags
        // into book_tags. ADR-0021 + ADR-0022.
        Arc::new(ab_llm_extractors::ExtractSettingStage::new(&tunables.llm)),
        // `extract-summary-spoiler-free-series` (slice 3K.4.1) —
        // per-series spoiler-free synopsis, regenerated when a
        // book completes its own summary AND identity-resolve
        // writes a `book_series` row. Picks the predominant
        // `books.language` across the series' books as the
        // output locale (ADR-0019). No `ai_cache` row — uses
        // the `series.summary_extractor_version` column added
        // by migration 007 for freshness.
        Arc::new(ab_llm_extractors::ExtractSeriesSummaryStage::new(
            &tunables.llm,
            &tunables.library_display,
        )),
    ];

    // `transcode-m4b` (ADR-0027) — Background-priority re-encode
    // of every active non-m4b source for a book into the
    // canonical m4b container. Runs in parallel with every other
    // stage; `book_file_refs` keeps sources alive while AI
    // consumers read them. The matching `PostTranscodeSourcesTarget`
    // cleanup target (registered in `build_cleanup_registry`)
    // reaps the source rows once refs settle.
    stages.push(Arc::new(ab_transcode::TranscodeM4bStage::new()));

    // `tag-write-early` (ADR-0028, Phase A2) — opt-in via
    // `tunables.pipeline.tag_write_early_enabled`. Default
    // `false` because flipping it on re-tags every book whose
    // winners differ from on-disk on the next pipeline pass;
    // that's a deliberate operator decision, not a fresh-
    // checkout default. Once enabled, every per-field
    // mutation logs to `mass_edit_history` with a per-run
    // batch_id (see PR #47).
    if tunables.pipeline.tag_write_early_enabled {
        // C3a: pass HTTP-client tunables through so the stage
        // can configure cover-art fetches with the project's
        // chosen timeout + byte cap. `from_tunables` fails
        // closed only on a broken TLS stack — degrade to the
        // permissive default constructor so the daemon stays
        // bootable in that pathological case.
        let early_stage =
            match ab_tag_write::TagWriteEarlyStage::from_tunables(&tunables.http_client) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "daemon.tag_write_early.cover_client_build_failed_using_default"
                    );
                    ab_tag_write::TagWriteEarlyStage::new()
                }
            };
        stages.push(Arc::new(early_stage));
        info!("daemon.pipeline.tag_write_early_enabled");
    } else {
        info!("daemon.pipeline.tag_write_early_disabled");
    }

    // `tag-write-final` (ADR-0028 § `TagWriteFinal`) — Background
    // priority, opt-in via `tunables.pipeline.tag_write_final_enabled`.
    // Same fallback shape as Early: `from_tunables` fails closed
    // only on a broken TLS stack; degrade to the permissive
    // default constructor so the daemon boots.
    //
    // The Final stage's `requires()` lists every AI extractor plus
    // `transcode-m4b`, so registering it makes the DAG build
    // verify those stages are still in the workspace at startup —
    // a stage rename surfaces here before any book runs through.
    if tunables.pipeline.tag_write_final_enabled {
        let final_stage =
            match ab_tag_write::TagWriteFinalStage::from_tunables(&tunables.http_client) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "daemon.tag_write_final.cover_client_build_failed_using_default"
                    );
                    ab_tag_write::TagWriteFinalStage::new()
                }
            };
        stages.push(Arc::new(final_stage));
        info!("daemon.pipeline.tag_write_final_enabled");
    } else {
        info!("daemon.pipeline.tag_write_final_disabled");
    }

    stages
}

#[tokio::main]
// `main` glues together every long-running task (signals, DAG,
// scheduler, three periodic loops, API + ABS servers). Each
// section is already extracted; further fragmentation hides flow
// more than it helps. Threshold cap is 100; we sit at ~102.
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init();

    info!(
        app = ab_core::build_info::APP_NAME,
        version = ab_core::build_info::VERSION,
        foreground = args.foreground,
        "daemon.start"
    );

    let storage_root = paths::app_support_dir();
    std::fs::create_dir_all(&storage_root).context("create storage root")?;

    // Acquire exclusive lock. Kernel releases on process exit; no
    // stale-detection logic needed.
    let _lock =
        lockfile::acquire(&storage_root.join("daemon.lock")).context("acquire daemon lockfile")?;

    // Layered config: defaults → <storage_root>/config.toml →
    // AB_* env. Per `ARCHITECTURE.md`, every tunable is meant to
    // be reachable from config; until this slice, the daemon was
    // hardcoded to `Tunables::default()` and silently ignored
    // operator config files. Failures are hard boot errors —
    // better to refuse to start than to drift from declared
    // operator intent.
    let tunables = Tunables::load(&storage_root).context("load tunables")?;
    if tunables.security.admin_token.is_none() {
        tracing::warn!(
            "daemon.start.no_admin_token — API runs unauthenticated. \
             Set `security.admin_token` in config.toml or AB_SECURITY__ADMIN_TOKEN \
             env to enable bearer-token auth."
        );
    }
    // B.7 (tracker #119): the `security.library_roots` Vec field
    // is gone. Authoritative storage lives in the DB-backed
    // `library_roots` table managed via the REST surface
    // (POST/GET/DELETE /api/v1/library_roots). The previous
    // one-cycle seed bridge from config.toml has been removed;
    // operators register roots through the API.

    // Open both databases. Pool sizing + busy-timeout come from
    // `tunables.db` (single source of truth in `ab_core::tunables`).
    let library = LibraryDb::open(&storage_root.join("library.db"), &tunables.db)
        .await
        .context("open library db")?;
    let ephemeral = EphemeralDb::open(&storage_root.join("ephemeral.db"), &tunables.db)
        .await
        .context("open ephemeral db")?;

    // Cancellation propagated to every worker on SIGTERM.
    let cancel = CancellationToken::new();
    spawn_signal_handlers(&cancel);

    // Build the pipeline DAG + scheduler.
    let stages = build_pipeline_stages(&tunables);
    let dag = Arc::new(Dag::build(stages).context("build pipeline DAG")?);
    let stage_ctx = StageContext {
        library: library.clone(),
        ephemeral: ephemeral.clone(),
        cancel: cancel.clone(),
        stage_name: "",
    };
    // Scheduler keeps its own Arc<Dag> to drive execution; we
    // also hand a clone to ApiState so handlers (notably the
    // retry endpoint, ADR-0023) can resolve user-supplied
    // stage strings into the typed StageId.
    let scheduler = Arc::new(Scheduler::spawn(
        Arc::clone(&dag),
        stage_ctx,
        &tunables.scheduler,
    ));

    // Idle-priority Speech-model installer (slice 3A.4.1) — drains
    // `pending_speech_installs` and re-queues blocked books.
    tokio::spawn(ab_transcript::run_idle_install_loop(
        ephemeral.clone(),
        Arc::clone(&scheduler),
        tunables.transcribe.clone(),
        cancel.clone(),
    ));

    // Periodic pipeline dispatcher (1F.A3). Sweeps newly-ready
    // (book, stage) pairs and reaps orphaned `running` rows.
    // Safety net for the synchronous auto-dispatch in
    // `Scheduler::execute`; idempotent on restart.
    tokio::spawn(scheduler.dispatcher_loop(
        library.clone(),
        ephemeral.clone(),
        tunables.scheduler.clone(),
        cancel.clone(),
    ));

    // Periodic cleanup loop (H.2.2/H.2.3, ADR-0025). Registry is
    // shared with the API surface so `aborg clean` and the
    // autonomous tick agree on scope.
    let cleanup = build_cleanup_registry(&tunables);
    spawn_cleanup_loop(
        &library,
        &ephemeral,
        &storage_root,
        &tunables,
        cleanup.clone(),
        &cancel,
    );
    // B.4: compile watch-folder exclusion globs at boot so every
    // scan + watchdog pass reuses the same matcher. Pattern
    // compilation errors are warned-and-dropped inside
    // `compile_excludes`.
    let scan_excludes = ab_scan::compile_excludes(&tunables.pipeline.scan_excludes);
    let api_state = ab_api::ApiState::new(
        library.clone(),
        ephemeral.clone(),
        scheduler,
        dag,
        cleanup,
        cancel.clone(),
        tunables.security.clone(),
        scan_excludes,
    );

    // Build the unified Router for the API port (api + webuis).
    let mut router = Router::new()
        .nest("/api/v1", ab_api::build_router(api_state))
        .nest("/config", ab_webui_config::build_router());
    #[cfg(feature = "player")]
    {
        router = router.nest("/player", ab_webui_player::build_router());
    }

    let api_addr: SocketAddr = format!("{}:{}", tunables.server.bind, tunables.server.api_port)
        .parse()
        .context("parse API bind address")?;

    info!(addr = %api_addr, "daemon.api.bind");

    let api_listener = tokio::net::TcpListener::bind(api_addr)
        .await
        .context("bind API listener")?;

    let api_serve = axum::serve(api_listener, router.into_make_service())
        .with_graceful_shutdown(cancel.clone().cancelled_owned());

    // Optionally serve the ABS-compat API on its own port.
    #[cfg(feature = "shelf")]
    let abs_serve = if tunables.server.abs_enabled {
        let abs_addr: SocketAddr = format!("{}:{}", tunables.server.bind, tunables.server.abs_port)
            .parse()
            .context("parse ABS bind address")?;
        info!(addr = %abs_addr, "daemon.abs.bind");
        let abs_listener = tokio::net::TcpListener::bind(abs_addr)
            .await
            .context("bind ABS listener")?;
        Some(
            axum::serve(
                abs_listener,
                ab_shelf::build_router(ab_shelf::ShelfState::new(library.clone()))
                    .into_make_service(),
            )
            .with_graceful_shutdown(cancel.clone().cancelled_owned()),
        )
    } else {
        None
    };

    // Run servers concurrently and wait for shutdown.
    #[cfg(feature = "shelf")]
    let result = if let Some(abs_serve) = abs_serve {
        tokio::try_join!(api_serve.into_future(), abs_serve.into_future()).map(|_| ())
    } else {
        api_serve.await
    };

    #[cfg(not(feature = "shelf"))]
    let result = api_serve.await;

    if let Err(e) = result {
        warn!(error = %e, "daemon.serve.error");
    }

    info!("daemon.stop");
    Ok(())
}

/// Spawn the periodic cleanup loop (slice H.2.2, ADR-0025).
///
/// Wakes every `tunables.check_secs`, computes the age cut-off from
/// disk pressure on `storage_root` via `statvfs`, and asks every
/// registered `CleanupTarget` to report what's eligible. v1 is
/// observability-only — `auto_apply = false` inside the loop;
/// operator applies via `aborg clean … --apply` (lands in H.2.3).
/// Build + spawn the periodic cleanup loop. Caller owns the
/// [`CleanupRegistry`] and a `&CancellationToken`; we clone what
/// we need.
#[allow(clippy::too_many_arguments)] // five strongly-typed inputs + cancel
fn spawn_cleanup_loop(
    library: &LibraryDb,
    ephemeral: &EphemeralDb,
    storage_root: &std::path::Path,
    tunables: &Tunables,
    registry: CleanupRegistry,
    cancel: &CancellationToken,
) {
    let ctx = CleanupLoopCtx {
        cleanup_ctx: CleanupCtx {
            library: library.clone(),
            ephemeral: ephemeral.clone(),
        },
        registry,
        tunables: tunables.cleanup.clone(),
        disk_free: disk_usage::disk_free_for(storage_root),
    };
    tokio::spawn(run_cleanup_loop(ctx, cancel.clone()));
}

/// Build the cleanup target registry. Each new target gets a line
/// here — `Arc::new(NewTarget)` is the entire wire job. Tunables
/// are threaded through so targets with operator-configurable
/// retention (e.g. `MassEditHistoryRetentionTarget`) read their
/// thresholds at registry-build time, not at every tick.
fn build_cleanup_registry(tunables: &Tunables) -> CleanupRegistry {
    let targets: Vec<Arc<dyn CleanupTarget>> = vec![
        // Queue: pairing codes past `expires_at` with no
        // `consumed_token_id`. Consumed rows survive as an audit
        // trail; only the dead-on-arrival pending codes go.
        Arc::new(ab_api::ExpiredPairingCodesTarget),
        // Db: mass-edit-history retention. 90-day window for the
        // latest row per (target_kind, target_id, field); 30-day
        // window for older intermediate rows. Both thresholds
        // configurable in `tunables.cleanup`.
        Arc::new(ab_tag_write::MassEditHistoryRetentionTarget::from_tunables(
            &tunables.cleanup,
        )),
        // Audio: source-file reaper for books that completed
        // their m4b transcode (ADR-0027). Gated on
        // `live_ref_count == 0` AND an active m4b row existing
        // for the book. Permanent no-op for books with no
        // transcode output.
        Arc::new(ab_transcode::PostTranscodeSourcesTarget::new()),
    ];
    CleanupRegistry::new(targets)
}

fn spawn_signal_handlers(cancel: &CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal as unix_signal};

    let c = cancel.clone();
    tokio::spawn(async move {
        if let Ok(mut sig) = unix_signal(SignalKind::terminate()) {
            sig.recv().await;
            info!("daemon.signal.sigterm");
            c.cancel();
        }
    });
    let c = cancel.clone();
    tokio::spawn(async move {
        if signal::ctrl_c().await.is_ok() {
            info!("daemon.signal.sigint");
            c.cancel();
        }
    });
}
