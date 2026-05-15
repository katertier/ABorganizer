//! Transcode-to-m4b stage (ADR-0027).
//!
//! Background-priority pipeline stage that decodes a book's
//! source file(s) and re-encodes them to canonical m4b via
//! AVFoundation (Swift FFI in [`ab_audio`] — actual encode
//! implementation is a TODO; this slice ships the stage
//! scaffolding + the refcount lifecycle that makes parallel
//! transcode-vs-AI safe).
//!
//! ## Scope of this slice
//!
//! - [`TranscodeM4bStage`] — Stage trait impl. `run()` is a stub
//!   that returns `Skipped` until the Swift `AVAssetExportSession`
//!   wrapper lands. The `requires()` set is empty so transcode
//!   can run in parallel with every other pipeline stage; sources
//!   stay alive via the [`ab_db::book_file_refs`] refcount.
//! - [`PostTranscodeSourcesTarget`] — `CleanupTarget` (ADR-0025)
//!   that reaps source files when a successful m4b output exists
//!   AND `live_ref_count == 0`. Both predicates ship live in this
//!   slice; the net effect is a permanent no-op until the Swift
//!   `AVAssetExportSession` writeback starts producing m4b rows
//!   in `book_files`, at which point the target activates without
//!   further code changes.

#![allow(missing_docs)] // scaffold-stage

pub mod cleanup;
pub(crate) mod output_resolve;
pub mod stage;

pub use cleanup::PostTranscodeSourcesTarget;
pub use stage::{STAGE_ID as TRANSCODE_M4B_STAGE_ID, TranscodeM4bStage};
