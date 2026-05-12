//! Pipeline stages that consume the Apple Intelligence
//! Foundation Models bridge.
//!
//! Each stage reads a cached transcript out of `ai_cache`,
//! builds a stage-specific prompt, calls
//! [`ab_foundation_models::complete`], parses the JSON the
//! model returns, and promotes the result into the library DB
//! (a column on `books`, a row in `book_tags`, a row in
//! `characters`, etc.).
//!
//! Shipped stages:
//!
//! - [`ExtractDnaTagsStage`] — `#`-prefixed thematic tags +
//!   `!`-prefixed spoiler tags into `book_tags`.
//!
//! Planned (slices 3K.4 / 3K.5 / 3K.6):
//!
//! - Spoiler-free summary into `books.summary_spoiler_free`.
//! - Story arc into `books.story_arc_json`.
//! - Characters into the `characters` table.
//!
//! All four follow the same pattern: idempotent re-runs
//! keyed by `LlmTunables.model_version` stamped on the
//! `ai_cache` row; user-fixable Foundation-Models failures
//! (Apple Intelligence disabled, device not eligible, model
//! not ready) propagate as `Err` so `aborg doctor llm` can
//! surface a fix-it, not as silent skips.

pub mod dna_stage;

pub use dna_stage::{
    ExtractDnaTagsStage, STAGE_NAME as EXTRACT_DNA_TAGS_STAGE, TAG_SOURCE_DNA_LLM,
    build_prompt as build_dna_prompt, normalise_tag,
};
