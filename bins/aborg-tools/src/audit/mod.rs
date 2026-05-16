//! Audiologo audit binary support modules (ADR-0054).
//!
//! The binary at `bins/aborg-tools/src/bin/audiologo_audit.rs`
//! is the CLI shim; the actual work happens here:
//!
//! * [`walk`] — corpus directory traversal + per-book metadata
//! * [`clips`] — ffmpeg-shell-out for 60s audio-clip extraction
//! * [`waveform`] — symphonia → PCM → SVG waveform renderer
//! * [`report`] — HTML emitter + embedded JS rating UI

pub mod clips;
pub mod match_seed;
pub mod report;
pub mod seed;
pub mod walk;
pub mod waveform;

/// A single book entry the audit pipeline produces for the report.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// Filesystem-safe slug derived from title or filename.
    pub slug: String,
    /// Display title — book's tag title, or filename without extension.
    pub title: String,
    /// Absolute source path.
    pub source_path: std::path::PathBuf,
    /// Total file duration in ms (lofty-reported).
    pub duration_ms: u64,
    /// Publisher tag if present (e.g. "Audible Studios",
    /// "Brilliance Audio"). Surfaces in the report as a
    /// corroborating signal for the audiologo decision —
    /// publisher identity strongly correlates with which jingle
    /// signature the operator should expect, so the audit lets
    /// the operator cross-check the detected method against the
    /// publisher.
    pub publisher: Option<String>,
    /// Copyright tag if present. Often agrees with publisher
    /// but sometimes holds the imprint when publisher is the
    /// parent label. Same role as publisher in the audit display.
    pub copyright: Option<String>,
    /// Detection result — for Phase 1, always
    /// [`DetectionInfo::Stub`]. Phase 2 wires the real pipeline.
    pub detection: DetectionInfo,
    /// 60s front-window clip path (relative to report `--out` dir).
    pub front_clip_rel: String,
    /// 60s end-window clip path (relative).
    pub end_clip_rel: String,
    /// Front waveform SVG inline content.
    pub front_waveform_svg: String,
    /// End waveform SVG inline content.
    pub end_waveform_svg: String,
    /// Optional "detail" clip extracted around the front-side
    /// seed-match offset (Phase 2D). 15s window centred on the
    /// match start; lets the operator confirm the match is real
    /// without scrubbing through the full 60s overview. `None`
    /// when no front-side match exists (no seed, no match, or
    /// pre-Phase-2C cached data).
    pub front_detail: Option<DetailClip>,
    /// Optional "detail" clip for the end side.
    pub end_detail: Option<DetailClip>,
}

/// Per-side detail clip + waveform for the two-clip layout
/// (Phase 2D). Sits next to the 60s overview so the operator
/// can listen to the focused window without scrubbing.
#[derive(Debug, Clone)]
pub struct DetailClip {
    /// Relative path to the .m4a clip from the report `--out` dir.
    pub clip_rel: String,
    /// Inline SVG waveform for the detail clip.
    pub waveform_svg: String,
    /// Start offset (ms) within the overview clip. Display label
    /// only — operator sees "starts at MM:SS in the 60s window".
    pub start_offset_in_overview_ms: u64,
    /// Duration of the detail clip in seconds.
    pub duration_secs: u32,
}

/// Per-book detection metadata for the report.
///
/// Phase 1 only emitted [`DetectionInfo::Stub`]. Phase 2C adds
/// [`DetectionInfo::SeedMatch`] for the cascade's first cut
/// (known publisher fingerprint hit). Future Phase 2 work wires
/// the full `DetectAudiologoStage` against an ephemeral DB to
/// fill in `Detected` with method + trigger + cut offsets when
/// no seed matches.
#[derive(Debug, Clone)]
pub enum DetectionInfo {
    /// Detection pipeline not yet wired into the audit binary.
    /// Operator can still rate the clips themselves. Used when
    /// no `--seed-fingerprints` were provided OR neither the
    /// front nor end clip matched any seed.
    Stub,
    /// Cascade's first cut: a known-publisher fingerprint from
    /// the seed DB matched the clip's chromaprint. The match
    /// includes confidence + which seed (so the operator can
    /// cross-reference against the seed's transcript excerpt).
    /// Front and end are independent — a book may match a known
    /// intro jingle without matching any outro.
    SeedMatch {
        /// Best front-clip match if any seed matched it.
        front: Option<SeedMatchSummary>,
        /// Best end-clip match if any seed matched it.
        end: Option<SeedMatchSummary>,
    },
    /// Pipeline ran; cut proposed at the given offsets.
    Detected {
        /// Detection method (the `Method` enum variant).
        method_label: String,
        /// Human-readable trigger context (e.g. matched phrase,
        /// silence span).
        trigger_summary: String,
        /// Proposed front cut offset (ms from file start).
        /// `None` if no front cut proposed.
        front_cut_ms: Option<u64>,
        /// Proposed end cut offset (ms from file start).
        /// `None` if no end cut proposed.
        end_cut_ms: Option<u64>,
    },
    /// Pipeline ran but produced no candidate (clean book; no
    /// detection methods fired above threshold).
    NoCandidate,
}

/// Display-friendly summary of a seed match. Lifted from
/// [`match_seed::SeedMatch`] so the report renderer doesn't
/// depend on that crate's exact field layout.
#[derive(Debug, Clone)]
pub struct SeedMatchSummary {
    /// Publisher tag from the matched seed.
    pub publisher: Option<String>,
    /// Confidence ∈ [0.0, 1.0].
    pub confidence: f32,
    /// Hamming distance at the best alignment offset.
    pub hamming: u32,
    /// Seed's needle length (chromaprint hashes).
    pub needle_hashes: usize,
    /// Hash-position offset inside the clip where the alignment
    /// begins (chromaprint hash unit ≈ 0.124 s).
    pub hash_offset: usize,
    /// Approximate offset within the clip in milliseconds
    /// (`hash_offset * 124`). Useful for human display + the
    /// future cut-mark insertion UI.
    pub approx_offset_ms: u64,
}

impl SeedMatchSummary {
    /// Build a summary from a [`match_seed::SeedMatch`].
    ///
    /// Lives here (not in `match_seed`) to keep the renderer-
    /// facing API decoupled from the matcher's internal types.
    #[must_use]
    pub fn from_match(m: &match_seed::SeedMatch) -> Self {
        Self {
            publisher: m.publisher.clone(),
            confidence: m.confidence,
            hamming: m.hamming,
            needle_hashes: m.needle_hashes,
            hash_offset: m.hash_offset,
            // chromaprint hash unit ≈ 0.1238 s; 124 ms is close
            // enough for human display.
            approx_offset_ms: (m.hash_offset as u64).saturating_mul(124),
        }
    }
}
