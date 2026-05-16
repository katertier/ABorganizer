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
}

/// Per-book detection metadata for the report.
///
/// Phase 1 only emits [`DetectionInfo::Stub`] — Phase 2 wires
/// the real `DetectAudiologoStage` against an ephemeral DB and
/// fills in `Detected` with method + trigger + cut offsets.
#[derive(Debug, Clone)]
pub enum DetectionInfo {
    /// Detection pipeline not yet wired into the audit binary.
    /// Phase 1 placeholder. Operator can still rate the clips
    /// from start + end of the file — useful for ground-truth
    /// "is there a jingle at all?" annotations.
    Stub,
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
