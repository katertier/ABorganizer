//! `extract-story-arc` pipeline stage (slice 3K.5).
//!
//! For each book this stage:
//!
//! 1. Loads the cached `transcript_full` row from `ai_cache`
//!    (written by `transcribe-full`). Skips if absent.
//! 2. Reads `books.language` — the output locale. Same rule as
//!    the spoiler-free summary stage: the arc stays in the
//!    book's native language regardless of `library_locale`
//!    (ADR-0019 / ADR-0022).
//! 3. Builds a prompt asking the on-device LLM for a 5-7 beat
//!    narrative arc, each beat carrying `{step, label,
//!    summary}`. The schema constraint (via
//!    [`ab_foundation_models::complete_structured`]) forces the
//!    model to emit `{arc: [...], arc_lang}` shape.
//! 4. Self-checks the response:
//!    - `arc_lang` must match `books.language` (primary
//!      subtag) — otherwise log warn + skip promotion.
//!    - Step numbers must be `1..=arc.len()` in order; reject
//!      otherwise so consumers don't have to defend against
//!      gaps or duplicates.
//! 5. Writes `books.story_arc_json` (column from migration
//!    003) with the array of beats serialised verbatim.
//! 6. Caches the raw response in `ai_cache` keyed
//!    `(book_id, "story_arc")` with `locale = books.language`
//!    and the current `extractor_version` for idempotency.
//!
//! ## Idempotency
//!
//! Skip when an `ai_cache` row exists at the current
//! `extractor_version`. Bump `LlmTunables::extractor_version` to
//! force re-extract library-wide; the `aborg book retry` CLI
//! (ADR-0023) covers per-book retries.
//!
//! ## Failure modes
//!
//! - No cached full transcript → `Skipped`.
//! - Transcript empty / below sanity floor → `Skipped`.
//! - `books.language` NULL → `Skipped`.
//! - Foundation Models unavailable → `Err`.
//! - Wrong locale / out-of-range step count → log warn + skip
//!   promotion (cache still written so re-runs at the same
//!   `extractor_version` are no-ops).
//! - Schema parse failure → `Err` (the schema is a
//!   compile-time constant; this should be unreachable).
//!
//! ## Spoiler handling
//!
//! Per ADR-0022, the first quarter of the beats (rounded up)
//! is always visible; later beats are gated by
//! `TagsTunables.show_spoiler_tags`. The gating happens at the
//! read surface (player UI / API); this stage writes the full
//! arc unconditionally.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ab_core::tunables::LlmTunables;
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use ab_foundation_models::{BridgeError, GenerationOptions, complete_structured};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("extract-story-arc");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// JSON Schema passed to `complete_structured`.
///
/// Constrains the model's output at decode time. The schema-
/// parity test below asserts this stays in lock-step with the
/// [`ArcResponse`] Rust shape — adding a field on one side
/// without the other fails at landing.
pub const ARC_SCHEMA_JSON: &str = r#"{
    "type": "object",
    "properties": {
        "arc": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "step": {"type": "integer"},
                    "label": {"type": "string"},
                    "summary": {"type": "string"}
                },
                "required": ["step", "label", "summary"]
            }
        },
        "arc_lang": {"type": "string"}
    },
    "required": ["arc", "arc_lang"]
}"#;

/// Stage that asks the on-device LLM for a 5-7 beat narrative
/// arc in the book's native language.
pub struct ExtractStoryArcStage {
    tunables: Arc<LlmTunables>,
}

impl ExtractStoryArcStage {
    /// Construct.
    #[must_use]
    pub fn new(tunables: &LlmTunables) -> Self {
        Self {
            tunables: Arc::new(tunables.clone()),
        }
    }
}

#[async_trait]
impl Stage for ExtractStoryArcStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Full transcript + summary: per ADR-0022 the summary
        // dependency sequences LLM calls per book. The summary
        // is also a cheap "did the language come out right?"
        // smoke-test before we burn tokens on the arc.
        &[
            ab_transcript::full_stage::STAGE_ID,
            crate::summary_stage::STAGE_ID,
        ]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // 1. Idempotency.
        if arc_cache_fresh(&ctx.library, book_id, &self.tunables.extractor_version).await? {
            return Ok(StageOutcome::Skipped);
        }

        // 2. Inputs: book language + full transcript.
        let Some(book_lang) = load_book_language(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };
        let Some(transcript) = load_full_transcript(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };
        if transcript.trim().len() < 200 {
            return Ok(StageOutcome::Skipped);
        }

        // 3. Call the bridge.
        let shape = PromptShape {
            step_words_low: self.tunables.arc_step_target_words_low,
            step_words_high: self.tunables.arc_step_target_words_high,
        };
        let prompt = build_prompt(&transcript, &book_lang, shape);
        // Story arc is a creative summary — leave temperature at
        // framework default for variety.
        let opts = GenerationOptions::new(self.tunables.arc_max_tokens);
        let raw = match complete_structured(&prompt, ARC_SCHEMA_JSON, &opts).await {
            Ok(s) => s,
            Err(BridgeError::PromptEmpty) => return Ok(StageOutcome::Skipped),
            Err(e) => return Err(bridge_to_stage_error(&e)),
        };

        // 4. Parse + validate.
        let parsed = parse_arc(&raw, book_id)?;
        let valid = validate_response(&parsed, &book_lang, book_id);
        if !valid {
            write_cache(
                &ctx.library,
                book_id,
                &raw,
                &book_lang,
                &self.tunables.extractor_version,
            )
            .await?;
            return Ok(StageOutcome::Skipped);
        }

        // 5. Promote to `books.story_arc_json` + write cache.
        let arc_json = serde_json::to_string(&parsed.arc)
            .map_err(|e| Error::stage(STAGE_NAME, format!("encode arc: {e}")))?;
        promote_arc(&ctx.library, book_id, &arc_json, &book_lang).await?;
        write_cache(
            &ctx.library,
            book_id,
            &raw,
            &book_lang,
            &self.tunables.extractor_version,
        )
        .await?;

        tracing::info!(
            book_id = book_id.0,
            lang = %book_lang,
            steps = parsed.arc.len(),
            "fm.arc.extracted"
        );
        Ok(StageOutcome::Done)
    }
}

/// Decode the model's JSON response. A parse failure is a
/// schema-vs-Rust drift and surfaces as `Err` so it shows up
/// in the daemon log; in production the `complete_structured`
/// schema constraint should make this unreachable.
fn parse_arc(raw: &str, book_id: BookId) -> Result<ArcResponse> {
    match serde_json::from_str::<ArcResponse>(raw) {
        Ok(p) => Ok(p),
        Err(e) => {
            tracing::warn!(
                book_id = book_id.0,
                error = %e,
                raw_len = raw.len(),
                "fm.arc.parse_failed"
            );
            Err(Error::stage(STAGE_NAME, format!("model JSON parse: {e}")))
        }
    }
}

/// Run the two self-checks (locale, step numbering). Returns
/// `true` when the response is safe to promote; the caller
/// still writes the cache row either way so re-runs at the
/// same `extractor_version` are idempotent.
fn validate_response(parsed: &ArcResponse, book_lang: &str, book_id: BookId) -> bool {
    let locale_ok = parsed.arc_lang.eq_ignore_ascii_case(book_lang)
        || primary_subtag(&parsed.arc_lang) == primary_subtag(book_lang);
    if !locale_ok {
        tracing::warn!(
            book_id = book_id.0,
            expected = %book_lang,
            got = %parsed.arc_lang,
            "fm.arc.locale_mismatch"
        );
        return false;
    }

    if !steps_are_well_formed(&parsed.arc) {
        tracing::warn!(
            book_id = book_id.0,
            steps = ?parsed.arc.iter().map(|b| b.step).collect::<Vec<_>>(),
            "fm.arc.step_numbering_invalid"
        );
        return false;
    }

    true
}

/// JSON shape produced by the LLM. `arc_lang` is a self-check
/// — the prompt asks for the book's BCP-47 tag and the locale-
/// mismatch path above rejects bad outputs.
#[derive(Debug, Deserialize, Serialize)]
struct ArcResponse {
    arc: Vec<ArcBeat>,
    arc_lang: String,
}

/// One beat in the narrative arc. Promoted verbatim into the
/// JSON-array stored in `books.story_arc_json`. The
/// `Serialize` impl is what `promote_arc` uses.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArcBeat {
    /// 1-indexed position in the arc; consumers iterate in
    /// `step` order. Validated `1..=arc.len()` before promotion.
    pub step: u32,
    /// Short (1-4 word) name of the beat in the book's
    /// language. `"Setup"` / `"Aufstellung"` / `"起承転結"`.
    pub label: String,
    /// 1-2 sentence beat description in the book's language.
    /// Target word range from the `arc_step_target_words_*`
    /// tunables.
    pub summary: String,
}

/// Verify that `arc[i].step == i + 1` for all `i`. Reject any
/// numbering that's not 1..=N in order; the UI's group-by-step
/// rendering depends on this invariant.
fn steps_are_well_formed(beats: &[ArcBeat]) -> bool {
    beats
        .iter()
        .enumerate()
        .all(|(i, b)| usize::try_from(b.step).is_ok_and(|s| s == i + 1))
}

/// Read the book's native language (BCP-47). NULL → not ready
/// — the transcribe-head-tail stage seeds this column from the
/// detected speech locale.
async fn load_book_language(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT language FROM books WHERE book_id = ?", id)
        .fetch_optional(library.pool())
        .await
        .map_err(|e| Error::Database(format!("arc load lang: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let trimmed = row
        .language
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(trimmed.map(str::to_owned))
}

/// Segment shape from the cached `transcript_full` row. Same
/// shape duplicated across DNA / summary / arc — keep
/// duplicated until a fourth call site shows up; then promote
/// to a shared helper.
#[derive(Debug, Deserialize)]
struct CachedTranscript {
    segments: Vec<Segment>,
}

#[derive(Debug, Deserialize)]
struct Segment {
    text: String,
}

/// Load the `transcript_full` cache row and concatenate the
/// segment text. Returns `None` when there's no row, the content
/// is missing, or the JSON shape is wrong.
async fn load_full_transcript(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let full_cache = CacheKey::TranscriptFull.as_str();
    let row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        full_cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("arc load transcript_full: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let cached: CachedTranscript = match ab_core::cache::deserialize_cache_content(&bytes) {
        Ok(c) => c,
        Err(e) => {
            // B.2a: covers JSON parse failures + oversized payloads.
            tracing::warn!(book_id = id, error = %e, "fm.arc.transcript_unparseable");
            return Ok(None);
        }
    };
    let mut text = String::with_capacity(cached.segments.iter().map(|s| s.text.len() + 1).sum());
    for seg in &cached.segments {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&seg.text);
    }
    Ok(Some(text))
}

async fn arc_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    extractor_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let arc_cache = CacheKey::StoryArc.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        arc_cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("arc cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    Ok(row.extractor_version.as_deref() == Some(extractor_version))
}

async fn promote_arc(
    library: &LibraryDb,
    book_id: BookId,
    arc_json: &str,
    _lang: &str,
) -> Result<()> {
    let id = book_id.0;
    // No `story_arc_lang` column on `books` — the per-beat
    // language is implied by `books.language`. The `_lang`
    // parameter is accepted for symmetry with summary's
    // signature; binding it would need a separate column
    // (deferred to a future migration if the arc UI ever
    // wants to render in a per-arc-fallback locale).
    sqlx::query!(
        "UPDATE books SET story_arc_json = ? WHERE book_id = ?",
        arc_json,
        id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("arc promote: {e}")))?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct CachePayload<'a> {
    raw: &'a str,
}

async fn write_cache(
    library: &LibraryDb,
    book_id: BookId,
    raw: &str,
    locale: &str,
    extractor_version: &str,
) -> Result<()> {
    let id = book_id.0;
    let payload = CachePayload { raw };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| Error::stage(STAGE_NAME, format!("encode cache: {e}")))?;
    let arc_cache = CacheKey::StoryArc.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, extractor_version, locale) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        arc_cache,
        bytes,
        extractor_version,
        locale,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("arc cache write: {e}")))?;
    Ok(())
}

/// Shape parameters for [`build_prompt`].
///
/// Bundled into a struct so future per-beat word-count tunables
/// don't add positional args. Both fields come from
/// `LlmTunables`; defaults live in `LlmTunables::default()`.
#[derive(Debug, Clone, Copy)]
pub struct PromptShape {
    /// Target floor for per-beat `summary` word count.
    pub step_words_low: usize,
    /// Target cap for per-beat `summary` word count.
    pub step_words_high: usize,
}

/// Build the prompt sent to the LLM. Public for unit-testing.
///
/// `transcript` is the concatenated `transcript_full` segments;
/// truncated defensively at 30k chars (same budget as the
/// summary stage — past that the on-device context cost rises
/// faster than the marginal arc signal).
///
/// Spoiler boundary: the arc covers the whole book, and the
/// later beats *do* contain plot resolutions. The UI gates
/// later beats behind `TagsTunables.show_spoiler_tags`; this
/// prompt does not ask the model to soften the later beats.
#[must_use]
pub fn build_prompt(transcript: &str, book_locale: &str, shape: PromptShape) -> String {
    const TRANSCRIPT_LIMIT: usize = 30_000;
    let excerpt = if transcript.len() > TRANSCRIPT_LIMIT {
        let mut end = TRANSCRIPT_LIMIT;
        while end > 0 && !transcript.is_char_boundary(end) {
            end -= 1;
        }
        &transcript[..end]
    } else {
        transcript
    };
    let PromptShape {
        step_words_low,
        step_words_high,
    } = shape;
    // Schema shape is conveyed by complete_structured with
    // includeSchemaInPrompt: true. We only state content rules.
    format!(
        "You are a literary analyst building a structural overview of an \
audiobook for a library browse view. Read the TRANSCRIPT below and produce \
a narrative arc.\n\
\n\
Rules:\n\
1. Output as many beats as the book's structure calls for. Each beat \
covers one structural movement (setup, inciting incident, rising action, \
climax, resolution, etc.). Use whatever beat-count gives the cleanest \
shape for THIS book.\n\
2. Number `step` from 1 to N in order; do not skip or duplicate numbers.\n\
3. `label` is a short (1-4 word) name for the beat in the book's language. \
Examples in English: \"Setup\", \"Inciting Incident\", \"Climax\".\n\
4. `summary` is {step_words_low}-{step_words_high} words describing what \
happens in that beat. Write tightly. This is where plot detail lives — be \
specific, even for later beats.\n\
5. Write in the book's native language. The book's BCP-47 locale is \
`{book_locale}`. Every `label` and `summary` MUST be in that language. \
Set `arc_lang` to `{book_locale}` to confirm.\n\
6. Cover the whole book. Do NOT soften or omit later-beat content; the UI \
hides spoilers downstream, not the model.\n\
\n\
TRANSCRIPT (locale={book_locale}):\n\
{excerpt}"
    )
}

/// BCP-47 primary subtag: `en-US` → `en`, `zh-Hans-CN` → `zh`.
fn primary_subtag(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

/// Map a bridge error into a Stage error.
fn bridge_to_stage_error(err: &BridgeError) -> Error {
    Error::stage(STAGE_NAME, format!("Foundation Models: {err}"))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn default_shape() -> PromptShape {
        PromptShape {
            step_words_low: 30,
            step_words_high: 50,
        }
    }

    #[test]
    fn build_prompt_includes_locale_words_and_transcript() {
        let p = build_prompt("Once upon a time…", "de", default_shape());
        assert!(p.contains("`de`"), "BCP-47 tag must appear in the prompt");
        assert!(p.contains("30-50 words"));
        assert!(p.contains("Once upon a time"));
        assert!(p.contains("locale=de"));
    }

    #[test]
    fn build_prompt_truncates_long_transcript() {
        let long = "x".repeat(40_000);
        let p = build_prompt(&long, "en", default_shape());
        assert!(p.len() < 32_000, "prompt len was {}", p.len());
    }

    #[test]
    fn parse_arc_response() {
        let json = r#"{
            "arc": [
                {"step": 1, "label": "Setup", "summary": "A young hero lives in obscurity."},
                {"step": 2, "label": "Call", "summary": "A mentor arrives with news."}
            ],
            "arc_lang": "en"
        }"#;
        let r: ArcResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.arc_lang, "en");
        assert_eq!(r.arc.len(), 2);
        assert_eq!(r.arc[0].step, 1);
        assert_eq!(r.arc[0].label, "Setup");
        assert_eq!(r.arc[1].step, 2);
    }

    #[test]
    fn steps_are_well_formed_accepts_canonical_sequence() {
        let beats = vec![
            ArcBeat {
                step: 1,
                label: "a".into(),
                summary: "x".into(),
            },
            ArcBeat {
                step: 2,
                label: "b".into(),
                summary: "y".into(),
            },
            ArcBeat {
                step: 3,
                label: "c".into(),
                summary: "z".into(),
            },
        ];
        assert!(steps_are_well_formed(&beats));
    }

    #[test]
    fn steps_are_well_formed_rejects_duplicate_step() {
        let beats = vec![
            ArcBeat {
                step: 1,
                label: "a".into(),
                summary: "x".into(),
            },
            ArcBeat {
                step: 1,
                label: "b".into(),
                summary: "y".into(),
            },
        ];
        assert!(!steps_are_well_formed(&beats));
    }

    #[test]
    fn steps_are_well_formed_rejects_skipped_step() {
        let beats = vec![
            ArcBeat {
                step: 1,
                label: "a".into(),
                summary: "x".into(),
            },
            ArcBeat {
                step: 3,
                label: "c".into(),
                summary: "z".into(),
            },
        ];
        assert!(!steps_are_well_formed(&beats));
    }

    #[test]
    fn steps_are_well_formed_rejects_zero_indexed() {
        let beats = vec![
            ArcBeat {
                step: 0,
                label: "a".into(),
                summary: "x".into(),
            },
            ArcBeat {
                step: 1,
                label: "b".into(),
                summary: "y".into(),
            },
        ];
        assert!(!steps_are_well_formed(&beats));
    }

    #[test]
    fn primary_subtag_normalises_bcp47() {
        assert_eq!(primary_subtag("en"), "en");
        assert_eq!(primary_subtag("en-US"), "en");
        assert_eq!(primary_subtag("zh-Hans-CN"), "zh");
        assert_eq!(primary_subtag(""), "");
    }

    /// Parity guard. `ARC_SCHEMA_JSON` is the constraint the
    /// framework enforces against the model's decode tokens.
    /// If the Rust shape drifts from the schema (a field renamed
    /// on one side, a new field added on the other), the runtime
    /// parse path masks it as a generic deserialisation error.
    /// This test makes the drift fail at landing.
    #[test]
    fn schema_parses_and_matches_response_shape() {
        let v: serde_json::Value = serde_json::from_str(ARC_SCHEMA_JSON).expect("schema parses");
        assert_eq!(v["type"], "object");
        let props = v["properties"]
            .as_object()
            .expect("properties is an object");

        // Top-level required fields match the ArcResponse struct.
        for field in ["arc", "arc_lang"] {
            assert!(
                props.contains_key(field),
                "schema missing top-level field `{field}`",
            );
        }
        let required = v["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|x| x.as_str().expect("required entry is string").to_owned())
            .collect::<Vec<_>>();
        assert!(required.contains(&"arc".to_owned()));
        assert!(required.contains(&"arc_lang".to_owned()));

        // `arc_lang` is a string.
        assert_eq!(props["arc_lang"]["type"], "string");

        // `arc` is an array; item shape matches ArcBeat.
        assert_eq!(props["arc"]["type"], "array");
        let item = &props["arc"]["items"];
        assert_eq!(item["type"], "object");
        let item_props = item["properties"]
            .as_object()
            .expect("item properties is an object");
        for (field, expected_ty) in [
            ("step", "integer"),
            ("label", "string"),
            ("summary", "string"),
        ] {
            let entry = item_props
                .get(field)
                .unwrap_or_else(|| panic!("schema missing arc.items.{field}"));
            assert_eq!(
                entry["type"], expected_ty,
                "arc.items.{field} must be `type: {expected_ty}` in schema",
            );
        }
        let item_required = item["required"]
            .as_array()
            .expect("arc.items.required is an array")
            .iter()
            .map(|x| {
                x.as_str()
                    .expect("item required entry is string")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        for field in ["step", "label", "summary"] {
            assert!(
                item_required.contains(&field.to_owned()),
                "arc.items.required missing `{field}`",
            );
        }
    }

    /// Beat content stays in the book's language; the locale-
    /// mismatch path skips promotion when the model emits a
    /// wrong-locale `arc_lang`. This test pins the prompt-level
    /// instruction (the runtime locale check is exercised via
    /// the integration tests).
    #[test]
    fn arc_prompt_instructs_book_language_for_labels_and_summaries() {
        let p = build_prompt("…", "de", default_shape());
        assert!(p.contains("Every `label` and `summary` MUST be in that language"));
        assert!(p.contains("Set `arc_lang` to `de`"));
    }
}
