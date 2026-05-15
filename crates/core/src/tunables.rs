//! Centralised configuration values.
//!
//! `Tunables` is the single source of truth for every tunable.
//! Resolution order:
//!
//!   1. CLI flag (highest priority)
//!   2. Environment variable (`AB_*`)
//!   3. Config TOML
//!   4. Default from this file (lowest)
//!
//! Doc comments on each field auto-generate `docs/CONFIG.md` via
//! `cargo xtask gen-config-docs`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Top-level tunables — everything user-configurable in the app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tunables {
    /// Persistent data root.
    pub storage_root: PathBuf,

    /// Network controls.
    pub network: NetworkTunables,

    /// HTTP server bindings + pairing.
    pub server: ServerTunables,

    /// Pipeline concurrency.
    pub pipeline: PipelineTunables,

    /// Audiologo detection knobs.
    pub audiologo: AudiologoTunables,

    /// Player defaults (overridable per-user in the UI).
    pub player: PlayerTunables,

    /// Logging.
    pub log: LogTunables,

    /// Database connection pool sizes + busy-timeout.
    pub db: DbTunables,

    /// Outbound HTTP client timeouts.
    pub http_client: HttpClientTunables,

    /// Scheduler internal channel buffer sizes.
    pub scheduler: SchedulerTunables,

    /// Tag-read stage (lofty-based file probe + tag extraction).
    pub tag_read: TagReadTunables,

    /// Tag presentation + export (genre `@`, DNA `#`, spoiler `!`
    /// prefix conventions and how they're surfaced to readers).
    pub tags: TagsTunables,

    /// Language detection (NLLanguageRecognizer) — pre- and
    /// post-transcribe gates.
    pub language: LanguageTunables,

    /// Head/tail transcribe stage (`SpeechAnalyzer` window sizes,
    /// `extractor_version` stamp).
    pub transcribe: TranscribeTunables,

    /// Apple Intelligence Foundation Models — token budgets +
    /// `extractor_version` stamp for the LLM-driven extractor stages
    /// (DNA tags, spoiler-free summary, story arc, characters).
    pub llm: LlmTunables,

    /// Library UI display locale (language names, genre
    /// translations, date / number formats). Distinct from any
    /// per-book language.
    pub library_display: LibraryDisplayTunables,

    /// Cleanup subsystem — periodic-loop interval + age-ratchet
    /// tiers. See [`CleanupTunables`] and ADR-0025.
    pub cleanup: CleanupTunables,

    /// Bearer-token authentication + path-validation roots.
    /// See [`SecurityTunables`].
    pub security: SecurityTunables,

    /// Background-task registry (ADR-0035) — scheduling tick
    /// interval, set `tick_secs = 0` to disable the loop.
    pub background: BackgroundTunables,

    /// Loudness measurement + optional ReplayGain-style gain
    /// on transcode (ADR-0041).
    pub loudness: LoudnessTunables,
}

impl Default for Tunables {
    fn default() -> Self {
        Self {
            storage_root: crate::paths::app_support_dir(),
            network: NetworkTunables::default(),
            server: ServerTunables::default(),
            pipeline: PipelineTunables::default(),
            audiologo: AudiologoTunables::default(),
            player: PlayerTunables::default(),
            log: LogTunables::default(),
            db: DbTunables::default(),
            http_client: HttpClientTunables::default(),
            scheduler: SchedulerTunables::default(),
            tag_read: TagReadTunables::default(),
            tags: TagsTunables::default(),
            language: LanguageTunables::default(),
            transcribe: TranscribeTunables::default(),
            llm: LlmTunables::default(),
            library_display: LibraryDisplayTunables::default(),
            cleanup: CleanupTunables::default(),
            security: SecurityTunables::default(),
            background: BackgroundTunables::default(),
            loudness: LoudnessTunables::default(),
        }
    }
}

/// Loudness measurement + optional transcode gain (ADR-0041).
///
/// Schema columns `books.lufs_integrated` / `books.lufs_truepeak`
/// are populated by the future `loudness-measure` stage (AVFoundation
/// EBU R-128 via Swift FFI). Tunables ship now so saved-queries +
/// transcode-gain can read them once measurements land. The stage
/// itself is wired in a follow-up slice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoudnessTunables {
    /// Target integrated loudness for the optional transcode-gain
    /// pass, in LUFS. Audiobook common target: -18 LUFS (matches
    /// Audible's published band). Set `apply_gain = false` to
    /// disable the gain stage entirely and treat the measurement
    /// as informational only.
    pub target_lufs: f32,
    /// True-peak ceiling in dBTP. The transcode-gain pass caps
    /// gain so the post-gain true-peak stays at or below this
    /// value. -1.0 dBTP is the common ceiling that survives
    /// lossy re-encoding.
    pub truepeak_ceiling_dbtp: f32,
    /// Enable the optional ReplayGain-style gain pass on
    /// transcode. False (default) — measurement only; the
    /// `lufs_*` columns get populated but no gain is applied.
    /// Flip to `true` to apply uniform loudness across the
    /// library at transcode time.
    pub apply_gain: bool,
}

impl Default for LoudnessTunables {
    fn default() -> Self {
        Self {
            target_lufs: -18.0,
            truepeak_ceiling_dbtp: -1.0,
            apply_gain: false,
        }
    }
}

/// Background-task registry knobs (ADR-0035).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackgroundTunables {
    /// How often the registry walks its task list. Set `0` to
    /// disable the loop entirely (manual triggers via API still
    /// work — the registry is shared with the handlers).
    pub tick_secs: u64,
}

impl Default for BackgroundTunables {
    fn default() -> Self {
        Self { tick_secs: 60 }
    }
}

/// Toggles for outbound network access.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkTunables {
    /// Allow Audnexus catalog lookups.
    pub audnexus_allowed: bool,
    /// Allow Audible search / product page scraping.
    pub audible_allowed: bool,
    /// Region order tried by Audnexus when ASIN-based lookup misses.
    pub audnexus_region_order: Vec<String>,
    /// Region order tried by `audible-search` when the home
    /// region returns no products. Same 2-letter codes as
    /// `audnexus_region_order` — each code maps to an
    /// `api.audible.<tld>` host inside `crates/catalog`.
    /// Stops on the first non-empty search response.
    pub audible_region_order: Vec<String>,
    /// Cover image fetch (Audnexus / Audible CDN).
    pub cover_fetch_allowed: bool,
}

impl Default for NetworkTunables {
    fn default() -> Self {
        Self {
            audnexus_allowed: true,
            audible_allowed: true,
            audnexus_region_order: vec![
                "us".into(),
                "uk".into(),
                "de".into(),
                "fr".into(),
                "ca".into(),
                "au".into(),
                "jp".into(),
                "in".into(),
                "it".into(),
            ],
            audible_region_order: vec![
                "us".into(),
                "uk".into(),
                "de".into(),
                "fr".into(),
                "ca".into(),
                "au".into(),
                "jp".into(),
                "in".into(),
                "it".into(),
            ],
            cover_fetch_allowed: true,
        }
    }
}

/// HTTP listeners + auth.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerTunables {
    /// Native API + web UIs port.
    pub api_port: u16,
    /// Audiobookshelf-compat API port. Set `enabled = false` to skip.
    pub abs_port: u16,
    /// Audiobookshelf-compat feature toggle.
    pub abs_enabled: bool,
    /// Bind interface (`127.0.0.1` for localhost-only, `0.0.0.0` for
    /// LAN access — pairing required).
    pub bind: String,
    /// Pairing-code TTL in seconds.
    pub pairing_ttl_secs: u64,
}

impl Default for ServerTunables {
    fn default() -> Self {
        Self {
            api_port: 8429,
            abs_port: 13378,
            abs_enabled: false,
            bind: "127.0.0.1".into(),
            pairing_ttl_secs: 600,
        }
    }
}

/// Pipeline / scheduler concurrency.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineTunables {
    /// Concurrent scan workers.
    pub scan_workers: usize,
    /// Concurrent network-bound enrichment workers.
    pub enrich_workers: usize,
    /// Concurrent transcribe workers (gated by Swift bridge).
    pub transcribe_workers: usize,
    /// Concurrent audio-CPU workers (fingerprint / audiologo / transcode).
    pub audio_workers: usize,
    /// Maximum pending jobs per stage before scan refuses new entries.
    pub max_pending_per_stage: usize,

    /// Opt-in switch for `tag-write-early` (ADR-0028).
    ///
    /// `false` (default) — the stage's body ships but is NOT
    /// wired into the pipeline DAG, so books never reach it.
    /// Flip to `true` after dry-running the lofty writes
    /// against a library copy; once on, every book whose
    /// `tag-read` + consensus winners differ from on-disk will
    /// be re-tagged on the next pipeline pass. Per-field
    /// before/after pairs land in `mass_edit_history` so a
    /// future undo surface can roll them back if needed.
    pub tag_write_early_enabled: bool,

    /// Opt-in switch for `tag-write-final` (ADR-0028).
    ///
    /// `false` (default) for the same reason as
    /// [`Self::tag_write_early_enabled`]: turning it on re-tags
    /// every book whose late winners (AI summary, story-arc,
    /// characters, setting) differ from on-disk. Operators
    /// flip both together once they've vetted the early-stage
    /// rewrites. Once on, the late pass skips per-field when
    /// the winner's source is `'user_edit'` (see
    /// `ab_tag_write::USER_EDIT_SOURCE`) so user corrections
    /// stay sticky across the AI cycle.
    pub tag_write_final_enabled: bool,

    /// Watch-folder exclusion globs applied during scan (B.4,
    /// tracker #119). Each pattern is a `globset`-compatible
    /// glob matched against either the file basename
    /// (`*.tmp`, `.DS_Store`) or any directory component in the
    /// file's path (`temp`, `sample`). A matching path is
    /// skipped during scan + watchdog walks before any
    /// `is_audio_file` test, so download-manager junk
    /// (partials, system metadata, sample dirs) never
    /// pollutes `book_files`.
    ///
    /// Defaults cover the empirically-common noise:
    /// `*.tmp`, `*.part`, `*.crdownload`, `.DS_Store`, `Thumbs.db`,
    /// `temp`, `sample`, `samples`. Operators can extend in
    /// config.toml; pattern compilation errors at boot are
    /// surfaced as a startup warning and the offending pattern
    /// is silently dropped from the active set.
    pub scan_excludes: Vec<String>,
}

impl Default for PipelineTunables {
    fn default() -> Self {
        Self {
            scan_workers: 4,
            enrich_workers: 4,
            transcribe_workers: 1,
            audio_workers: 4,
            max_pending_per_stage: 10_000,
            tag_write_early_enabled: false,
            tag_write_final_enabled: false,
            scan_excludes: vec![
                "*.tmp".into(),
                "*.part".into(),
                "*.crdownload".into(),
                ".DS_Store".into(),
                "Thumbs.db".into(),
                "temp".into(),
                "sample".into(),
                "samples".into(),
            ],
        }
    }
}

/// Audiologo detection knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudiologoTunables {
    /// Intro scan window (seconds).
    pub intro_window_secs: f64,
    /// Outro scan window (seconds).
    pub outro_window_secs: f64,
    /// Silence-detect noise floor (dBFS).
    pub silence_noise_db: f32,
    /// Minimum silence run length to count as a boundary (seconds).
    pub silence_min_secs: f64,
    /// Pre-content headroom retained when cutting (seconds).
    pub cut_headroom_secs: f64,

    // ── Auto-apply confidence floors (per ADR-0024 Revision 2) ──
    //
    // Slice 4B.5 reads these to decide whether a candidate row
    // produced by `detect-audiologo` should auto-promote to
    // `applied` (with chapter shift) or stay at `candidate` for
    // user review. Transcript-bearing tiers ship with 0.0 floors
    // → they never auto-apply by design; the user reviews every
    // transcript-aided candidate (4D review UI).
    /// Auto-apply floor for `Method::FingerprintFull`.
    pub fp_full_min_confidence: f32,
    /// Auto-apply floor for `Method::FingerprintBookend`.
    pub fp_bookend_min_confidence: f32,
    /// Auto-apply floor for `Method::FingerprintAndTranscript`.
    /// Set to 0.0 to keep the tier as candidate-only.
    pub fp_and_transcript_min_confidence: f32,
    /// Auto-apply floor for `Method::TranscriptOnly`. Set to
    /// 0.0 to keep the tier as candidate-only.
    pub transcript_only_min_confidence: f32,

    // ── Splice padding (ms) ───────────────────────────────────
    //
    // Slice 4B.5 inserts this much silence after the cut to
    // soften mid-utterance splices. Used as the default when
    // `book_file_audiologos.padding_ms` is NULL.
    /// Padding after an intro cut (ms).
    pub intro_padding_ms: u32,
    /// Padding after an outro cut (ms).
    pub outro_padding_ms: u32,
}

impl Default for AudiologoTunables {
    fn default() -> Self {
        Self {
            intro_window_secs: 120.0,
            outro_window_secs: 60.0,
            silence_noise_db: -40.0,
            silence_min_secs: 0.6,
            cut_headroom_secs: 0.3,
            fp_full_min_confidence: 0.85,
            fp_bookend_min_confidence: 0.80,
            fp_and_transcript_min_confidence: 0.0,
            transcript_only_min_confidence: 0.0,
            intro_padding_ms: 250,
            outro_padding_ms: 250,
        }
    }
}

/// Player defaults (UI exposes per-user overrides).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlayerTunables {
    /// Single-tap skip-back, seconds.
    pub skip_back_secs: u32,
    /// Single-tap skip-forward, seconds.
    pub skip_forward_secs: u32,
    /// Auto-rewind on resume after a pause longer than `pause_threshold_secs`.
    pub jumpback_secs: u32,
    /// Pause duration above which jumpback applies.
    pub pause_threshold_secs: u32,
    /// Variable playback speed (0.5 to 3.0). Stored as multiplier.
    pub default_speed: f32,
}

impl Default for PlayerTunables {
    fn default() -> Self {
        Self {
            skip_back_secs: 30,
            skip_forward_secs: 30,
            jumpback_secs: 10,
            pause_threshold_secs: 5,
            default_speed: 1.0,
        }
    }
}

/// Logging destinations + verbosity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogTunables {
    /// Enable file logs at `~/Library/Logs/<DisplayName>/`.
    pub file_enabled: bool,
    /// Per-rotated file size in MB.
    pub file_rotation_mb: u64,
    /// Number of rotated files retained.
    pub file_rotation_count: u32,
    /// Tracing directive (`info,ab_pipeline=debug` etc.).
    pub level: String,
}

impl Default for LogTunables {
    fn default() -> Self {
        Self {
            file_enabled: false,
            file_rotation_mb: 10,
            file_rotation_count: 5,
            level: "info".into(),
        }
    }
}

/// Database connection pool sizing + busy-timeout. Applied at
/// `LibraryDb::open` / `EphemeralDb::open` construction time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbTunables {
    /// Maximum concurrent connections to `library.db`. WAL allows
    /// many readers; the daemon owns the single writer.
    pub library_pool_max: u32,
    /// Maximum concurrent connections to `ephemeral.db`. Smaller
    /// because job-queue churn doesn't need wide read parallelism.
    pub ephemeral_pool_max: u32,
    /// SQLite `busy_timeout` (milliseconds). Time a query will wait
    /// on a busy lock before failing with `SQLITE_BUSY`.
    pub busy_timeout_ms: u64,
}

impl Default for DbTunables {
    fn default() -> Self {
        Self {
            library_pool_max: 8,
            ephemeral_pool_max: 4,
            busy_timeout_ms: 5_000,
        }
    }
}

/// Outbound HTTP client request timeouts. Applied to the reusable
/// `reqwest::Client` instances at construction time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpClientTunables {
    /// Per-request timeout for Audnexus calls (seconds).
    pub audnexus_timeout_secs: u64,
    /// Per-request timeout for Audible product-page scrapes (seconds).
    /// Generous; Audible pages can be slow.
    pub audible_timeout_secs: u64,
    /// Per-request timeout for the seed-data signed manifest fetch
    /// (seconds). Small JSON payload; short timeout fine.
    pub seed_timeout_secs: u64,
    /// Per-request timeout for cover-art image fetches (seconds).
    /// Covers come from Audible / Audnexus CDNs; default is on
    /// the high side because some CDN edges are slow.
    pub cover_fetch_timeout_secs: u64,
    /// Hard cap on cover-art payload size in bytes. Audible
    /// covers are typically ~300 KB; the cap is generous to
    /// allow for higher-res masters but defends against a
    /// hostile / misconfigured CDN feeding us a 200 MB image.
    pub cover_max_bytes: u64,
}

impl Default for HttpClientTunables {
    fn default() -> Self {
        Self {
            audnexus_timeout_secs: 15,
            audible_timeout_secs: 20,
            seed_timeout_secs: 10,
            // Cover fetch defaults: 30 s window + 5 MB cap.
            cover_fetch_timeout_secs: 30,
            cover_max_bytes: 5 * 1024 * 1024,
        }
    }
}

/// Pipeline scheduler internal mpsc-channel buffer sizes. Larger
/// buffers absorb burst submissions without blocking; the durable
/// job queue (`ephemeral.db.jobs`) is the real backpressure mechanism.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerTunables {
    /// Buffer for the interactive-priority channel.
    pub interactive_buffer: usize,
    /// Buffer for the background-priority channel.
    pub background_buffer: usize,
    /// Buffer for the idle-priority channel.
    pub idle_buffer: usize,
    /// Seconds the interactive + background queues must both have
    /// been empty before the idle queue starts draining. Set
    /// lower (e.g. 30) when there's a user UI watching, higher
    /// (e.g. 600) on a headless daemon. The starting default is a
    /// compromise; revisit once we have real-usage telemetry.
    pub idle_wait_secs: u64,
    /// Periodic dispatcher tick interval (seconds). Slice 1F.A3:
    /// the daemon spawns a loop that wakes every
    /// `dispatcher_check_secs` to (a) reap `pipeline_progress`
    /// rows for books that no longer exist or have no active
    /// files, and (b) sweep books that have an eligible
    /// next-stage waiting and submit one each at Background
    /// priority. 60 s default — fast enough that a new book
    /// from scan starts moving within a minute; slow enough
    /// that an empty queue costs negligible CPU. Set to 0 to
    /// disable the loop entirely (daemon-wiring path checks
    /// this).
    pub dispatcher_check_secs: u64,
    /// Cap on how many `(book, stage)` submissions the
    /// dispatcher can fan out per tick. Bounds worst-case work
    /// on a freshly-imported large library so a single tick
    /// can't blast thousands of jobs into the background queue
    /// at once. The next tick (one `dispatcher_check_secs`
    /// later) picks up where this one stopped.
    pub dispatcher_max_submissions_per_tick: usize,
}

impl Default for SchedulerTunables {
    fn default() -> Self {
        Self {
            interactive_buffer: 128,
            background_buffer: 4_096,
            idle_buffer: 4_096,
            // 5 min of quiet before idle work starts. Conservative
            // starting value — long enough that someone manually
            // dropping a few books in the queue doesn't get
            // overtaken by hours of full-transcript work; short
            // enough that an unattended daemon doesn't sit idle
            // forever.
            idle_wait_secs: 300,
            dispatcher_check_secs: 60,
            // 256 ≈ "biggest sustained library import that
            // still keeps each tick under a second of DB work
            // on the dev mini." Operational tuning; bump if a
            // tick takes >5s on a real workload.
            dispatcher_max_submissions_per_tick: 256,
        }
    }
}

/// Tag-read stage knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TagReadTunables {
    /// Insert `book_field_provenance` candidates for every tag
    /// field we successfully extract. Off → only the per-file
    /// audio properties (duration / bitrate / codec) are written.
    pub write_provenance: bool,
}

impl Default for TagReadTunables {
    fn default() -> Self {
        Self {
            write_provenance: true,
        }
    }
}

/// Tag presentation + export semantics.
///
/// `book_tags` rows are written with a one-character prefix that
/// classifies the tag's nature:
///
/// | Prefix | Meaning | Stored | Player / ABS default |
/// |---|---|---|---|
/// | `@` | Genre tag (`@fantasy`, `@thriller`). Mirrors a row in `genres` for canonical display name + hierarchy. | always | shown |
/// | `#` | DNA tag — safe to display (`#cozy`, `#unreliable-narrator`, `#commute-friendly`). | always | shown |
/// | `!` | Spoiler DNA tag (`!hero-dies`, `!magic-system-revealed-chapter-12`). | always | hidden by default |
///
/// Filter / export semantics live entirely in this struct; the
/// storage layer is identical for all three prefixes. That lets
/// the spoiler-hide flag toggle uniformly across the player UI,
/// the native API, the ABS-compat API, and file-tag writeback.
///
/// Similarity / "books like this" queries always use the full set
/// — the prefixes only control what reaches the reader's eyes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TagsTunables {
    /// Include `!`-prefixed (spoiler) tags in the player UI, API
    /// responses, ABS-compat output, and file-tag write-back.
    /// When `false`, spoiler tags are filtered everywhere a
    /// reader sees them but remain in storage for similarity
    /// queries. Default `false`.
    pub show_spoiler_tags: bool,
    /// Keep prefix characters (`@`, `#`, `!`) in exported tag
    /// strings (ABS API, file-tag writes). When `false`, prefixes
    /// are stripped before export — useful for cleaner display in
    /// downstream tools but lossy across round-trips. Default
    /// `true` for round-trip fidelity.
    pub export_tag_prefix: bool,
}

impl Default for TagsTunables {
    fn default() -> Self {
        Self {
            show_spoiler_tags: false,
            export_tag_prefix: true,
        }
    }
}

/// Library-wide locale used for UI display strings.
///
/// Covers language names, genre translations (future slice),
/// date / number formatting. Stored as BCP-47 primary subtag
/// (e.g. `"en"`, `"de"`); not region-aware in v0. Independent
/// of any per-book language.
///
/// Where it's read:
///
/// - `language_code::display_name` will eventually use it to
///   localise output (v0 always returns English names).
/// - The future genre-translation slice will key its lookup
///   table on this value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryDisplayTunables {
    /// BCP-47 primary subtag for the library UI language.
    /// Default `"en"` — most ABorganizer adopters are
    /// English-speaking. Override in `config.toml` for a German
    /// / French / etc. library.
    pub library_locale: String,
}

impl Default for LibraryDisplayTunables {
    fn default() -> Self {
        Self {
            library_locale: "en".into(),
        }
    }
}

/// Language detection knobs (`NLLanguageRecognizer` via Swift FFI).
///
/// Two call paths:
///
/// - **Pre-transcribe**: feed concatenated tag text to pick the
///   `SpeechTranscriber` locale. Doesn't need a skip; tag text
///   has no jingles.
/// - **Post-transcribe validation**: feed transcript segments
///   past the publisher-jingle window. The skip matters here —
///   Audible + most publishers run an English house jingle in
///   the first ~30 s regardless of book language, which biases
///   short non-English samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageTunables {
    /// Drop transcript segments ending before this offset (ms)
    /// when running post-transcribe language detection. Captures
    /// the Audible house jingle (~6 s) plus typical publisher
    /// branding (15–30 s). 30 000 ms is the conservative default
    /// that covers both. Pre-transcribe detection (tag-text)
    /// ignores this.
    pub post_transcribe_skip_ms: u64,
    /// Number of alternatives to ask `NLLanguageRecognizer` for
    /// beyond the dominant hit. Stored alongside the chosen
    /// language so downstream extractors can see how close the
    /// runner-up was (low margin → less trust in the locale).
    pub max_alternatives: usize,
    /// Minimum confidence on the dominant hypothesis before we
    /// commit to a detected locale. Below this, we fall back to
    /// the default locale (`default_locale`).
    pub min_confidence: f64,
    /// Locale used when detection is inconclusive or unavailable
    /// (no tag text, framework error, below-threshold confidence).
    /// BCP-47.
    pub default_locale: String,
    /// Minimum input length (chars) before we even attempt
    /// detection. `NLLanguageRecognizer` is unreliable on <16
    /// chars; below this we skip and fall back to default.
    pub min_text_chars: usize,
}

impl Default for LanguageTunables {
    fn default() -> Self {
        Self {
            // 30 s — Audible jingle ~6 s, publisher branding can
            // push to 20–25 s on some imprints; 30 is the cushion.
            post_transcribe_skip_ms: 30_000,
            max_alternatives: 3,
            // 0.65 is where NLLanguageRecognizer's separation
            // between top-2 hypotheses tends to settle into
            // "actually confident, not coin-flip." Verified on
            // empirical ABtagger samples — see ROADMAP "Language
            // detection thresholds."
            min_confidence: 0.65,
            default_locale: "en-US".into(),
            min_text_chars: 16,
        }
    }
}

/// Head/tail transcribe stage knobs.
///
/// The stage transcribes two windows per book: `[0, head_secs)`
/// for downstream extractors (audiologo, language, title/author
/// confirm, DNA, summary) and `[duration - tail_secs, duration)`
/// for outro audiologo + last-sentence boundary work. Results
/// land in `ai_cache` keyed by `(book_id, cache_type)` with
/// `cache_type` ∈ {`transcript_head`, `transcript_tail`} and
/// the `extractor_version` stamp below.
///
/// Why store `extractor_version`: when Apple ships a new
/// `SpeechAnalyzer` engine, bump this and the stage re-runs
/// (the cached row is stale). Derived features that DON'T need
/// the new engine (e.g. re-running language detection with a
/// tweaked tunable) re-read the same cached transcript without
/// re-transcribing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranscribeTunables {
    /// Length of the head transcription window in seconds.
    /// 6 minutes is the sweet spot — covers the Audible jingle
    /// (~6 s), publisher branding (~30 s), author/title intro
    /// (typically <2 min), and the start of the prologue/first
    /// chapter. Past 6 min, marginal extractor value drops.
    pub head_secs: f64,
    /// Length of the tail transcription window in seconds.
    /// 30 s captures the outro publisher jingle + closing
    /// credits ("This has been an Audible production…")
    /// without paying for non-jingle book content.
    pub tail_secs: f64,
    /// Engine identifier written to `ai_cache.extractor_version`.
    /// Bump to force re-transcription across the library when
    /// the Speech framework improves materially. The string is
    /// opaque to the engine — it's a content-addressable cache
    /// key on our side. Convention: `speech-<macOS>-v<bump>`.
    pub extractor_version: String,
    /// Skip the stage when the active file is shorter than this
    /// (seconds). Below ~30 s neither head nor tail are useful;
    /// the file is probably a sample / preview / corrupt entry.
    pub min_duration_secs: f64,
    /// Idle-priority Speech-model installer wake interval
    /// (seconds). The daemon spawns one tokio task at startup
    /// that wakes every `idle_install_check_secs` to drain
    /// `pending_speech_installs`. 1800 s (30 min) is the
    /// default — fast enough that a freshly-imported book in a
    /// new locale gets transcribed within an hour; slow enough
    /// that an empty queue costs negligible CPU.
    pub idle_install_check_secs: u64,
    /// Positions through the book at which the
    /// `transcribe-samples` stage takes short windows for
    /// language confirmation + fast DNA-tag corpus. Fractions
    /// in `(0.0, 1.0)`. Default `[0.25, 0.50, 0.75]` — deep
    /// enough that publisher jingles (0%) and outro material
    /// (≥95%) are clear; spread across the book so a single
    /// chapter-boundary intro can't bias all three samples.
    pub sample_positions: Vec<f64>,
    /// Length of each sample (seconds) the `transcribe-samples`
    /// stage transcribes. 60 s is plenty for
    /// `NLLanguageRecognizer` and a representative DNA-tag
    /// corpus. Total transcribed audio per book =
    /// `sample_positions.len() * sample_secs`.
    pub sample_secs: f64,
}

impl Default for TranscribeTunables {
    fn default() -> Self {
        Self {
            head_secs: 360.0,
            tail_secs: 30.0,
            extractor_version: "speech-26.0-v1".into(),
            min_duration_secs: 30.0,
            idle_install_check_secs: 1_800,
            sample_positions: vec![0.25, 0.50, 0.75],
            sample_secs: 60.0,
        }
    }
}

/// Token budgets + model version stamp for the LLM-driven
/// extractor stages backed by Apple Intelligence's Foundation
/// Models framework.
///
/// `extractor_version` is the cache-invalidation key written to
/// `ai_cache.extractor_version` for every row produced by an LLM
/// stage. Bump it (e.g. `fm-26.0-v1` → `fm-26.0-v2`) to force
/// every book re-extract — useful after a prompt rewrite, after
/// Apple ships a major Foundation Models update, or after a
/// schema change that breaks the cached JSON shape.
///
/// The per-stage `*_max_tokens` knobs are soft budgets passed
/// straight to `GenerationOptions.maximumResponseTokens`. The
/// framework treats them as upper bounds; EOS can land earlier.
/// Defaults are tuned for typical first-5-minute transcript
/// excerpts (DNA tags + summary + arc + characters all fit
/// comfortably).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmTunables {
    /// Version stamp written to `ai_cache.extractor_version` for
    /// every LLM-produced row. Opaque to the runtime — it's a
    /// cache key on our side. Convention: `fm-<macOS>-v<bump>`.
    pub extractor_version: String,
    /// Token budget for the DNA-tag extractor (`#`-prefixed
    /// safe-to-display tags + `!`-prefixed spoiler tags).
    /// 800 tokens is plenty for a JSON array of 5-10 tags each
    /// 1-3 words long; bump if you raise the per-list caps.
    pub dna_max_tokens: usize,
    /// Maximum `#`-prefixed DNA tags per book. The prompt
    /// instructs the model to stop at this count; downstream
    /// also truncates defensively in case the model overruns.
    /// 8 is the sweet spot — enough for "books like this"
    /// similarity, few enough to scan visually.
    pub dna_max_tags: usize,
    /// Maximum `!`-prefixed spoiler tags. Smaller cap by design
    /// — most books have one or two real spoilers; the model
    /// over-eagerly marks anything plot-bearing as a spoiler if
    /// given a generous budget.
    pub dna_max_spoiler_tags: usize,
    /// Token budget for the spoiler-free summary extractor.
    /// 600 tokens lands a 3-5 paragraph summary in any of the
    /// five UI locales without truncation.
    pub summary_max_tokens: usize,
    /// Soft floor for the summary word count. The prompt tells
    /// the model to target this many words minimum; the actual
    /// floor is enforced loosely (we don't reject sub-floor
    /// outputs — too dry but still useful). Empirically 100 is
    /// the point below which summaries lose tone signal.
    pub summary_target_words_low: usize,
    /// Soft cap for the summary word count. The prompt tells
    /// the model to target this many words maximum; output is
    /// not truncated post-generation (the model decides where to
    /// stop). Empirically 150 is the point above which summaries
    /// start drifting into spoiler territory.
    pub summary_target_words_high: usize,
    /// Token budget for the story-arc extractor. JSON array of
    /// `{step, label, summary}` rows — 1200 tokens covers a
    /// typical 5-7 act arc with 1-2 sentence summaries each.
    pub arc_max_tokens: usize,
    /// Soft floor for the number of arc beats the prompt asks
    /// for. Schema-constrained generation can't enforce array
    /// lengths, so the prompt restates the range and the parse
    /// path rejects out-of-range outputs. 5 beats is the
    /// classical Freytag floor; fewer loses narrative shape.
    pub arc_target_steps_low: usize,
    /// Soft cap for the number of arc beats. 7 beats is the
    /// modal upper end for a single-protagonist novel; beyond
    /// that the model starts producing chapter-by-chapter
    /// summaries instead of a true arc.
    pub arc_target_steps_high: usize,
    /// Soft floor for words per arc beat's `summary` field.
    /// 30 words is enough for one sentence of narrative
    /// signal; below that the beats become labels-only.
    pub arc_step_target_words_low: usize,
    /// Soft cap for words per arc beat's `summary` field.
    /// 50 words is two sentences max; beyond that the
    /// per-beat summary starts revealing plot resolutions
    /// the UI hides by default (later-stage beats are
    /// spoiler-gated, see ADR-0022's spoiler-handling row).
    pub arc_step_target_words_high: usize,
    /// Token budget for the character extractor. JSON array of
    /// `{name, aliases, role, description}` rows — 1500 tokens
    /// covers up to ~15 characters with brief descriptions.
    pub characters_max_tokens: usize,
    /// Soft cap for the number of characters the prompt asks
    /// for. 12 is the modal upper end for an audiobook cast:
    /// 3-5 principals + a handful of recurring secondaries.
    /// Schema-constrained generation can't enforce the array
    /// length; the prompt restates the cap and the runtime
    /// truncates defensively.
    pub characters_max: usize,
    /// Soft floor for the per-character `description` word
    /// count. 20 words is two sentences max; below that the
    /// descriptions lose tone signal.
    pub character_desc_target_words_low: usize,
    /// Soft cap for the per-character `description` word
    /// count. 40 words is the point above which descriptions
    /// start incorporating plot beats that belong in the arc
    /// or summary stages.
    pub character_desc_target_words_high: usize,
    /// Token budget for the setting extractor. Has to cover
    /// a 30-60 word paragraph PLUS up to ~25 `$`-prefixed
    /// tags across 10 categories — 800 tokens is the floor
    /// that fits both without truncating the tag list.
    pub setting_max_tokens: usize,
    /// Soft floor for the setting paragraph word count.
    /// 30 words is two sentences. Less than that and the
    /// paragraph reads as a label, not a description.
    pub setting_target_words_low: usize,
    /// Soft cap for the setting paragraph word count.
    /// 60 words is four sentences max. Beyond that the
    /// paragraph drifts into plot territory; the `$`-tag
    /// array carries the structured signal.
    pub setting_target_words_high: usize,
    /// Soft cap on the total number of `$`-prefixed setting
    /// tags emitted per book (sum across all 10 categories).
    /// 25 absorbs faction-heavy books (`$group-*` blooms);
    /// the prompt restates the cap and the runtime truncates
    /// defensively.
    pub setting_max_tags: usize,
}

impl Default for LlmTunables {
    fn default() -> Self {
        Self {
            extractor_version: "fm-26.0-v1".into(),
            dna_max_tokens: 800,
            dna_max_tags: 8,
            dna_max_spoiler_tags: 4,
            summary_max_tokens: 600,
            summary_target_words_low: 100,
            summary_target_words_high: 150,
            arc_max_tokens: 1_200,
            arc_target_steps_low: 5,
            arc_target_steps_high: 7,
            arc_step_target_words_low: 30,
            arc_step_target_words_high: 50,
            characters_max_tokens: 1_500,
            characters_max: 12,
            character_desc_target_words_low: 20,
            character_desc_target_words_high: 40,
            setting_max_tokens: 800,
            setting_target_words_low: 30,
            setting_target_words_high: 60,
            setting_max_tags: 25,
        }
    }
}

/// Cleanup-subsystem tunables (slice H.2, ADR-0025).
///
/// The daemon spawns a periodic loop that wakes every
/// `check_secs`, asks each registered `CleanupTarget` to
/// `report` what's eligible under the current age tier, then
/// `apply` (if the loop is configured for auto-apply; v1 ships
/// dry-run only — operator runs `aborg clean ... --apply` to
/// actually delete).
///
/// `default_age_days` is the baseline gate; under disk
/// pressure the loop walks `pressure` tiers and picks the
/// smallest matching `age_days`. Both `free_percent` and
/// `free_bytes` are valid triggers per tier (the operator
/// picks whichever framing fits their hardware — % on small
/// laptops, absolute bytes on NAS rigs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupTunables {
    /// Wake interval of the periodic cleanup loop, in seconds.
    /// `0` disables the loop (operator-triggered cleanups via
    /// `aborg clean` still work). Default `3600` (1 h).
    pub check_secs: u64,
    /// Baseline age gate in days. Items older than this are
    /// eligible to be cleaned during a non-pressure tick.
    /// Default `14`.
    pub default_age_days: u64,
    /// Pressure tiers, evaluated in order. Each tier specifies
    /// at least one trigger (`free_percent` or `free_bytes`)
    /// and the `age_days` to apply when triggered. Tiers
    /// stack: the smallest matching `age_days` across all hit
    /// tiers wins (most-aggressive cleanup).
    pub pressure: Vec<PressureTier>,
    /// Retention window in days for the **latest** row per
    /// `(target_kind, target_id, field)` tuple in
    /// `mass_edit_history`. Once a row crosses this age and
    /// it's still the most recent edit for its key, the
    /// retention target marks it eligible for prune.
    ///
    /// Default `90`. Tuned to keep recent state long enough
    /// for an operator to manually undo a mass-edit gone
    /// wrong (the audit-trail's primary user-facing job),
    /// without indefinitely growing the table.
    pub mass_edit_history_latest_days: u64,
    /// Retention window in days for **intermediate** rows in
    /// `mass_edit_history` — every row that isn't the most
    /// recent edit for its key. After this age they're
    /// eligible for prune regardless of disk pressure.
    ///
    /// Default `30`. Tighter than `latest_days` because
    /// intermediate rows aren't reachable by the undo flow
    /// once a newer edit has shadowed them; they exist only
    /// for forensic audit and a month is generous for that.
    pub mass_edit_history_intermediate_days: u64,
}

/// One ratchet step in [`CleanupTunables::pressure`].
///
/// Both threshold fields are optional but at least one should
/// be set or the tier is dead config (silently skipped — no
/// validation panic). The double-knob design covers both
/// framings:
///
/// - `free_percent` — natural for a single-disk laptop (10 %,
///   5 %).
/// - `free_bytes`   — natural for a NAS or anything where %
///   becomes finicky at scale (a 22 TB volume at 5 % free is
///   still 1.1 TB, well past sane thresholds).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PressureTier {
    /// Trigger when free disk % drops below this. `None` to
    /// disable the percent trigger on this tier.
    pub free_percent: Option<f32>,
    /// Trigger when free disk bytes drops below this. `None`
    /// to disable the absolute trigger on this tier.
    pub free_bytes: Option<u64>,
    /// Age gate (in days) to apply when this tier triggers.
    pub age_days: u64,
}

impl Default for PressureTier {
    fn default() -> Self {
        Self {
            free_percent: None,
            free_bytes: None,
            // Match `default_age_days` so an
            // unspecialised-via-serde tier is a no-op rather
            // than an aggressive surprise.
            age_days: 14,
        }
    }
}

impl Default for CleanupTunables {
    fn default() -> Self {
        Self {
            check_secs: 3_600,
            default_age_days: 14,
            // Two-tier ratchet — % and absolute thresholds set
            // on each tier. The first-hit-wins logic means
            // either framing triggers the same tier; on
            // wildly-different disk sizes the operator
            // overrides via config.
            pressure: vec![
                PressureTier {
                    free_percent: Some(10.0),
                    // 50 GB free is a reasonable floor on
                    // smaller dev rigs; NAS-grade users
                    // override.
                    free_bytes: Some(50 * 1024 * 1024 * 1024),
                    age_days: 7,
                },
                PressureTier {
                    free_percent: Some(5.0),
                    free_bytes: Some(10 * 1024 * 1024 * 1024),
                    age_days: 3,
                },
            ],
            mass_edit_history_latest_days: 90,
            mass_edit_history_intermediate_days: 30,
        }
    }
}

/// Security knobs: bearer-token auth + path-validation roots.
///
/// Both fields default to "disabled" so a fresh checkout keeps
/// the existing dev ergonomics — but the daemon's startup logs
/// a `warn` line for each `None` field at boot so operators
/// running on a non-loopback bind know what they're missing.
///
/// Set via `config.toml`:
///
/// ```toml
/// [security]
/// admin_token = "abc123..."  # 32+ random bytes hex-encoded
/// ```
///
/// Or via env: `AB_SECURITY_ADMIN_TOKEN=...`.
///
/// **B.7 removal note:** the `library_roots` Vec field that
/// previously seeded the DB-backed `library_roots` table was
/// dropped in slice B.7 (tracker #119). The one-cycle bridge has
/// served its purpose — operators manage roots through the
/// `library_roots` REST surface (POST / GET / DELETE). Any stale
/// `library_roots = [...]` setting in `config.toml` is now an
/// `unknown_field` error under `#[serde(deny_unknown_fields)]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityTunables {
    /// Bearer token the API layer compares against the
    /// `Authorization: Bearer <token>` header. `None` disables
    /// auth entirely — only safe on loopback binds. A future
    /// slice graduates to the per-user tokens table; this is
    /// the v0 hard-stop until that lands.
    pub admin_token: Option<String>,
}

impl Tunables {
    /// Load tunables from a layered config: built-in defaults
    /// → `<storage_root>/config.toml` (if it exists) → `AB_*`
    /// env (figment's `__` nested separator: e.g.
    /// `AB_SECURITY__ADMIN_TOKEN`).
    ///
    /// `storage_root` is used both as the lookup root for
    /// `config.toml` AND wins over the default `storage_root`
    /// field if no config file override sets it. Callers
    /// (`aborg-daemon::main`) pass the resolved `app_support`
    /// directory.
    ///
    /// Failures (malformed TOML, env coercion errors) return
    /// `Err` with figment's diagnostic chain attached. The
    /// daemon treats this as a hard boot error — better to
    /// refuse to start than to silently fall back to defaults
    /// that don't match operator intent.
    ///
    /// # Errors
    ///
    /// Returns [`figment::Error`] wrapped in
    /// [`crate::Error::Config`] on any merge / coercion failure.
    pub fn load(storage_root: &std::path::Path) -> crate::Result<Self> {
        use figment::Figment;
        use figment::providers::{Env, Format, Serialized, Toml};

        let config_path = storage_root.join("config.toml");
        let mut figment = Figment::from(Serialized::defaults(Self {
            storage_root: storage_root.to_path_buf(),
            ..Self::default()
        }));
        if config_path.exists() {
            figment = figment.merge(Toml::file(&config_path));
        }
        // Env layer last so AB_* always wins. `__` is figment's
        // documented nested-key separator.
        figment = figment.merge(Env::prefixed("AB_").split("__"));

        figment.extract().map_err(|e| {
            crate::Error::Config(format!(
                "tunables load (config={}): {e}",
                config_path.display(),
            ))
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::Tunables;

    #[test]
    fn defaults_round_trip_through_toml() {
        let default = Tunables::default();
        let serialized = toml::to_string_pretty(&default).expect("serialize");
        let parsed: Tunables = toml::from_str(&serialized).expect("parse");
        // Spot-check a few fields rather than implementing PartialEq.
        // Float comparison is exact here because we round-trip the
        // same default value through TOML — no math, no precision
        // loss. Allow the lint locally.
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(parsed.server.api_port, default.server.api_port);
            assert_eq!(
                parsed.audiologo.intro_window_secs,
                default.audiologo.intro_window_secs
            );
            assert_eq!(parsed.player.skip_back_secs, default.player.skip_back_secs);
        }
    }

    #[test]
    fn build_info_constants_are_populated() {
        // Generated values must be non-empty after build.rs runs.
        assert!(!crate::build_info::APP_NAME.is_empty());
        assert!(!crate::build_info::DISPLAY_NAME.is_empty());
        assert!(crate::build_info::BUNDLE_ID_BASE.contains('.'));
    }
}
