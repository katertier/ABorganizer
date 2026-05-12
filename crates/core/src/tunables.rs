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
    /// `model_version` stamp).
    pub transcribe: TranscribeTunables,

    /// Library UI display locale (language names, genre
    /// translations, date / number formats). Distinct from any
    /// per-book language.
    pub library_display: LibraryDisplayTunables,
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
            library_display: LibraryDisplayTunables::default(),
        }
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
}

impl Default for PipelineTunables {
    fn default() -> Self {
        Self {
            scan_workers: 4,
            enrich_workers: 4,
            transcribe_workers: 1,
            audio_workers: 4,
            max_pending_per_stage: 10_000,
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
}

impl Default for AudiologoTunables {
    fn default() -> Self {
        Self {
            intro_window_secs: 120.0,
            outro_window_secs: 60.0,
            silence_noise_db: -40.0,
            silence_min_secs: 0.6,
            cut_headroom_secs: 0.3,
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
}

impl Default for HttpClientTunables {
    fn default() -> Self {
        Self {
            audnexus_timeout_secs: 15,
            audible_timeout_secs: 20,
            seed_timeout_secs: 10,
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
/// the `model_version` stamp below.
///
/// Why store `model_version`: when Apple ships a new
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
    /// Engine identifier written to `ai_cache.model_version`.
    /// Bump to force re-transcription across the library when
    /// the Speech framework improves materially. The string is
    /// opaque to the engine — it's a content-addressable cache
    /// key on our side. Convention: `speech-<macOS>-v<bump>`.
    pub model_version: String,
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
    /// Per-chunk window size (seconds) for the full-book
    /// transcribe stage. Retained for the Rust-side chunking
    /// path until the Swift `AVAssetReader` rewrite lands;
    /// after that, the rewrite drops Rust-side chunks entirely
    /// and this field can be removed. 300 s (5 min) keeps peak
    /// PCM RAM ~20 MB.
    pub full_chunk_secs: f64,
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
            model_version: "speech-26.0-v1".into(),
            min_duration_secs: 30.0,
            idle_install_check_secs: 1_800,
            full_chunk_secs: 300.0,
            sample_positions: vec![0.25, 0.50, 0.75],
            sample_secs: 60.0,
        }
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
