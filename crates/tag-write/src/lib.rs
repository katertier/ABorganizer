//! Two-pass embedded-tag writer (ADR-0028).
//!
//! The pipeline produces metadata on two distinct timescales:
//!
//! 1. **5-minute DNA tags** — `tag-read` + `identity-resolve` +
//!    `extract-dna-tags`. Visible to the user within minutes of
//!    import.
//! 2. **Long-running AI outputs** — `extract-summary-spoiler-free`,
//!    `extract-story-arc`, `extract-characters`, `extract-setting`,
//!    `consensus`, future franchise / spoiler-free description.
//!    Accumulate over minutes-to-hours.
//!
//! Writing every tag only at the end of the pipeline leaves the
//! user staring at empty / stale tags during the AI window.
//! Writing every field on every change races user edits made via
//! the web UI. The decision (ADR-0028): two distinct stages, with
//! the late stage gated by `book_field_provenance.source !=
//! 'user_edit'` per-field.
//!
//! ## Scope of this slice
//!
//! Scaffolding only. The two `Stage` impls
//! ([`TagWriteEarlyStage`] / [`TagWriteFinalStage`]) ship as
//! `Skipped` skeletons — they exist so [`Stage::requires`] graphs
//! that name `tag-write-early` / `tag-write-final` upstream can
//! reference the live `StageId` constants. The `lofty`-based
//! file-write integration, the on-disk-match dedup, the m4b /
//! mp3 / flac per-format dispatcher, and the cover-art write path
//! land in follow-up slices.
//!
//! What ships here:
//!
//! - [`winners`] — the `SELECT` that pulls every winning row from
//!   `book_field_provenance` for one book. Used by both stages.
//! - [`USER_EDIT_SOURCE`] + [`skip_for_final_pass`] — the
//!   convention string + a tiny predicate that
//!   [`TagWriteFinalStage`] consults per-field. The string lives
//!   in one place so the eventual `record_user_edit()` helper in
//!   `ab-api` and the future typed `Source` enum (BACKLOG.md)
//!   stay in sync.
//! - [`stage::TagWriteEarlyStage`] / [`stage::TagWriteFinalStage`]
//!   — the two Stage impls. `name()`, `requires()`, and the
//!   typed `STAGE_ID` constants are real; `run()` is a no-op.
//!
//! Not registered in `aborg-daemon`'s pipeline registry yet —
//! per the slice cadence, skeletons stay invisible to operators
//! until they actually do work.

#![allow(missing_docs)] // scaffold-stage

pub mod cleanup;
pub mod stage;
pub mod winners;
pub mod write;

pub use cleanup::MassEditHistoryRetentionTarget;

/// Provenance-source convention for tags written via the web UI's
/// metadata-edit endpoint (`PATCH /api/v1/books/{id}`).
///
/// Per ADR-0028 § "`Source::User`" — `book_field_provenance.source`
/// is intentionally free-form text (no `CHECK` constraint, per
/// migration 011's rationale). The convention lives in one place
/// so every writer (web UI, future App Intents, voice) and every
/// reader (`TagWriteFinalStage`'s per-field skip) stay in sync
/// without a typo.
///
/// A typed enum `core::provenance::Source` is deliberately
/// **not** added at this slice — it would require touching every
/// existing provenance writer. BACKLOG.md tracks the typed-enum
/// graduation as a future slice.
pub const USER_EDIT_SOURCE: &str = "user_edit";

/// True if [`TagWriteFinalStage`] should leave a field's
/// on-disk tag untouched.
///
/// The rule (ADR-0028 § "Skips per-field on user-edit"): when a
/// field's current winner has `source = 'user_edit'`, the late
/// pass does NOT overwrite. The user's correction wins until they
/// explicitly clear it via the same UI surface.
///
/// `TagWriteEarlyStage` runs before any AI extractor produces
/// alternatives, so user-edit can't compete with anything at that
/// point — this predicate is final-stage-only by design.
#[must_use]
pub fn skip_for_final_pass(winner_source: &str) -> bool {
    winner_source == USER_EDIT_SOURCE
}

pub use stage::{
    TAG_WRITE_EARLY_STAGE_ID, TAG_WRITE_FINAL_STAGE_ID, TagWriteEarlyStage, TagWriteFinalStage,
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn user_edit_source_is_the_canonical_string() {
        assert_eq!(USER_EDIT_SOURCE, "user_edit");
    }

    #[test]
    fn skip_for_final_pass_matches_only_user_edit() {
        assert!(skip_for_final_pass("user_edit"), "exact match wins");
        assert!(!skip_for_final_pass("audnexus-enrich"));
        assert!(!skip_for_final_pass("tag_file"));
        assert!(!skip_for_final_pass("extract-summary-spoiler-free"));
        assert!(!skip_for_final_pass(""), "empty != user_edit");
        assert!(
            !skip_for_final_pass("USER_EDIT"),
            "case-sensitive — convention is exact lowercase"
        );
    }
}
