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
}

impl Default for SchedulerTunables {
    fn default() -> Self {
        Self {
            interactive_buffer: 128,
            background_buffer: 4_096,
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
