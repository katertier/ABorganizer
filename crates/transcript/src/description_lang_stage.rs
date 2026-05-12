//! `detect-description-lang` pipeline stage (slice 3G).
//!
//! Reads `books.description`, runs `NLLanguageRecognizer` on
//! the text, writes the canonical language code to
//! `books.description_lang`. Populates the column added by
//! slice 3D.1.
//!
//! ## Why a dedicated stage
//!
//! Description language is a derived property: `consensus`
//! picks the winning description value first; only then can we
//! detect what language it's in. Folding into consensus itself
//! would mix two concerns (winner-picking + NL detection). A
//! separate `requires=["consensus"]` stage keeps both simple.
//!
//! ## Idempotency
//!
//! Skips when `books.description_lang` is already populated.
//! Description text changing (e.g. consensus promotes a new
//! source) doesn't auto-re-detect; a future invalidate step
//! would clear the column when the description churns. In
//! practice the description is in one language per book.
//!
//! ## Failure modes
//!
//! - No `books` row → `Skipped`.
//! - `books.description` NULL or too short (<
//!   `LanguageTunables.min_text_chars`) → `Skipped`.
//! - `NLLanguageRecognizer` returns no hypothesis → `Skipped`.
//! - Detection confidence below
//!   `LanguageTunables.min_confidence` → `Skipped` (don't
//!   commit a low-signal guess to the column).

use std::sync::Arc;

use async_trait::async_trait;

use ab_core::tunables::LanguageTunables;
use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use ab_speech::detect;

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("detect-description-lang");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// `consensus` is the canonical writer of `books.description`;
/// we depend on it so the column is populated before we read.
const REQUIRES: &[StageId] = &[ab_catalog::consensus::STAGE_ID];

/// Per-book description language detector.
pub struct DetectDescriptionLangStage {
    language: Arc<LanguageTunables>,
}

impl DetectDescriptionLangStage {
    /// Construct.
    #[must_use]
    pub fn new(language: &LanguageTunables) -> Self {
        Self {
            language: Arc::new(language.clone()),
        }
    }
}

#[async_trait]
impl Stage for DetectDescriptionLangStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        REQUIRES
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let Some(text) =
            load_description(&ctx.library, book_id, self.language.min_text_chars).await?
        else {
            return Ok(StageOutcome::Skipped);
        };

        let Some(detection) = detect(&text, self.language.max_alternatives).await? else {
            return Ok(StageOutcome::Skipped);
        };

        // Normalise via the central language-code table — same
        // canonical form the rest of the pipeline uses
        // (`book_field_provenance.value` for language, etc.).
        let Some(canonical) = ab_core::language_code::normalize(&detection.language) else {
            tracing::warn!(
                raw = %detection.language,
                book = %book_id,
                "detect_description_lang.unparseable"
            );
            return Ok(StageOutcome::Skipped);
        };

        // Require minimum confidence — otherwise we'd persist
        // a low-signal guess into the column and any UI surface
        // would render with the wrong directionality / font.
        if detection.confidence < self.language.min_confidence {
            tracing::info!(
                book = %book_id,
                confidence = detection.confidence,
                threshold = self.language.min_confidence,
                "detect_description_lang.below_threshold"
            );
            return Ok(StageOutcome::Skipped);
        }

        write_description_lang(&ctx.library, book_id, &canonical).await?;
        Ok(StageOutcome::Done)
    }
}

/// Load `books.description` when it's populated AND
/// `description_lang` is still NULL AND the description is
/// long enough to detect on. Returns `None` for any skip
/// condition.
async fn load_description(
    library: &LibraryDb,
    book_id: BookId,
    min_chars: usize,
) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT description, description_lang FROM books WHERE book_id = ?",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("description-lang load: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    if row.description_lang.is_some() {
        // Idempotent: already populated.
        return Ok(None);
    }
    let Some(text) = row.description else {
        return Ok(None);
    };
    if text.chars().count() < min_chars {
        return Ok(None);
    }
    Ok(Some(text))
}

async fn write_description_lang(
    library: &LibraryDb,
    book_id: BookId,
    canonical: &str,
) -> Result<()> {
    let id = book_id.0;
    sqlx::query!(
        "UPDATE books SET description_lang = ? WHERE book_id = ?",
        canonical,
        id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("description-lang write: {e}")))?;
    Ok(())
}
