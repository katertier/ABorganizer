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
//! - [`ExtractSummaryStage`] — spoiler-free book summary into
//!   `books.summary_spoiler_free` + `_lang`.
//! - [`ExtractSeriesSummaryStage`] — spoiler-free series synopsis
//!   into `series.summary` + `_lang` (regenerated when a book
//!   joins a series or member-book summaries change).
//!
//! Planned (slices 3K.5 / 3K.6):
//!
//! - Story arc into `books.story_arc_json`.
//! - Characters into the `characters` table.
//!
//! All extractors follow the same pattern: idempotent re-runs
//! keyed by `LlmTunables.extractor_version` stamped on the
//! `ai_cache` row; user-fixable Foundation-Models failures
//! (Apple Intelligence disabled, device not eligible, model
//! not ready) propagate as `Err` so `aborg doctor llm` can
//! surface a fix-it, not as silent skips. The newer ones
//! (summary, future arc / characters) use
//! [`ab_foundation_models::complete_structured`] with a
//! `DynamicGenerationSchema` so the model can't emit
//! off-schema tokens; the DNA stage was retrofitted to the
//! same pattern in slice C5.7.d.

pub mod dna_stage;
pub mod series_summary_stage;
pub mod summary_stage;

pub use dna_stage::{
    DNA_SCHEMA_JSON, ExtractDnaTagsStage, STAGE_NAME as EXTRACT_DNA_TAGS_STAGE, TAG_SOURCE_DNA_LLM,
    build_prompt as build_dna_prompt, normalise_tag,
};
pub use series_summary_stage::{
    ExtractSeriesSummaryStage, SERIES_SUMMARY_SCHEMA_JSON,
    STAGE_NAME as EXTRACT_SERIES_SUMMARY_STAGE,
};
pub use summary_stage::{
    ExtractSummaryStage, STAGE_NAME as EXTRACT_SUMMARY_STAGE, SUMMARY_SCHEMA_JSON,
    build_prompt as build_summary_prompt,
};
