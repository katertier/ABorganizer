//! `extract-dna-tags` pipeline stage (slice 3K.3).
//!
//! For each book this stage:
//!
//! 1. Loads the cached `transcript_full` row from `ai_cache`
//!    (written by `transcribe-full`). Skips if absent.
//! 2. Builds an instruction prompt — "given this transcript
//!    excerpt, return a JSON object with two arrays: `dna_tags`
//!    (safe-to-display thematic tags) and `spoiler_tags`
//!    (plot-revealing tags)."
//! 3. Calls [`ab_foundation_models::complete_structured`]
//!    against Apple Intelligence's on-device LLM, passing a
//!    `DynamicGenerationSchema`-shaped JSON schema so the framework
//!    constrains the model's output to `{dna_tags: [string],
//!    spoiler_tags: [string]}` at decode time. (Retrofitted from
//!    the free-form `complete()` + parse-retry pattern in
//!    slice C5.7.c.)
//! 4. Parses the JSON, applies the configured per-category
//!    caps, and writes one row per tag to `book_tags` with
//!    `source = "dna_llm"` and the prefix convention
//!    (`#<tag>` for DNA, `!<tag>` for spoilers).
//! 5. Caches the raw response payload (extractor_version-stamped)
//!    in `ai_cache` keyed `(book_id, "dna_tags")` so a re-run
//!    at the same model version is idempotent.
//!
//! ## Idempotency
//!
//! Skip when an `ai_cache` row exists at the current
//! `extractor_version`. Bump the version (`LlmTunables::extractor_version`)
//! to force re-extract across the library.
//!
//! ## Failure modes
//!
//! - No cached transcript → `Skipped`. The transcribe-full
//!   stage seeds it; we don't transcribe ourselves.
//! - Transcript empty / below sanity floor → `Skipped`.
//! - Foundation Models unavailable
//!   ([`BridgeError::BridgeUnavailable`] / `AppleIntelligenceDisabled`
//!   / `DeviceNotEligible`) → `Err`. Per project policy these
//!   are user-fixable issues surfaced by `aborg doctor llm`,
//!   not silent skips.
//! - Model returned malformed JSON → log warning + `Err`. With
//!   schema-constrained generation this should be near-impossible
//!   (the model literally can't emit off-schema tokens), but
//!   the parse step + warn is kept as a defence-in-depth.
//! - [`BridgeError::SchemaParseFailure`] / `SchemaUnsupportedShape`
//!   → `Err`. Both indicate a bug in [`DNA_SCHEMA_JSON`], which
//!   is a `const` — should be impossible at runtime, but the
//!   typed variants surface clearly if a future schema edit
//!   regresses.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ab_core::tunables::LlmTunables;
use ab_core::{BookId, CacheKey, Error, Result, TagKind};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use ab_foundation_models::{BridgeError, complete_structured};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("extract-dna-tags");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// `book_tags.source` for rows produced by this stage.
pub const TAG_SOURCE_DNA_LLM: &str = "dna_llm";

/// JSON Schema passed to `complete_structured`.
///
/// Constrains the model's output at decode time. Maps to a
/// `DynamicGenerationSchema` on the Swift side; matches the
/// `DnaResponse` Rust shape one-to-one so any drift surfaces as a
/// `serde_json` parse error in the test suite (`parse_dna_response`).
///
/// `additionalProperties` is omitted — the bridge's
/// `buildDynamicSchema` ignores it (Apple's schema model doesn't
/// have a direct equivalent), and the schema-constrained decoder
/// can't emit unlisted keys anyway.
pub const DNA_SCHEMA_JSON: &str = r#"{
    "type": "object",
    "properties": {
        "dna_tags": {"type": "array", "items": {"type": "string"}},
        "spoiler_tags": {"type": "array", "items": {"type": "string"}}
    },
    "required": ["dna_tags", "spoiler_tags"]
}"#;

/// Stage that asks the on-device LLM for thematic DNA tags +
/// spoiler tags, then promotes them into `book_tags`.
pub struct ExtractDnaTagsStage {
    tunables: Arc<LlmTunables>,
}

impl ExtractDnaTagsStage {
    /// Construct.
    #[must_use]
    pub fn new(tunables: &LlmTunables) -> Self {
        Self {
            tunables: Arc::new(tunables.clone()),
        }
    }
}

#[async_trait]
impl Stage for ExtractDnaTagsStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // The full transcript is the input. The stage reads
        // transcript_full out of ai_cache — typed dep ensures the
        // transcribe-full stage's STAGE_ID stays in sync.
        &[ab_transcript::full_stage::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // 1. Idempotency.
        if dna_cache_fresh(&ctx.library, book_id, &self.tunables.extractor_version).await? {
            return Ok(StageOutcome::Skipped);
        }

        // 2. Load transcript.
        let Some(transcript) = load_full_transcript(&ctx.library, book_id).await? else {
            return Ok(StageOutcome::Skipped);
        };
        // Defensive sanity floor — under 200 chars it's
        // overwhelmingly an audiologo jingle without book content,
        // and the LLM will hallucinate. Below this threshold the
        // upstream transcribe stage should have already gated us
        // out, but the floor protects against tag races.
        if transcript.text.trim().len() < 200 {
            return Ok(StageOutcome::Skipped);
        }

        // 3. Build prompt + call the bridge.
        let prompt = build_prompt(
            &transcript.text,
            &transcript.locale,
            self.tunables.dna_max_tags,
            self.tunables.dna_max_spoiler_tags,
        );
        let raw = match complete_structured(&prompt, DNA_SCHEMA_JSON, self.tunables.dna_max_tokens)
            .await
        {
            Ok(s) => s,
            Err(BridgeError::PromptEmpty) => {
                // Should be impossible given the sanity floor
                // above — treat as a skip rather than a hard fail.
                return Ok(StageOutcome::Skipped);
            }
            Err(e) => return Err(bridge_to_stage_error(&e)),
        };

        // 4. Parse + apply caps.
        let parsed: DnaResponse = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    book_id = book_id.0,
                    error = %e,
                    raw_len = raw.len(),
                    "fm.dna.parse_failed"
                );
                return Err(Error::stage(STAGE_NAME, format!("model JSON parse: {e}")));
            }
        };
        let dna = clamp(&parsed.dna_tags, self.tunables.dna_max_tags);
        let spoilers = clamp(&parsed.spoiler_tags, self.tunables.dna_max_spoiler_tags);

        // 5. Write tags + cache.
        write_tags(&ctx.library, book_id, &dna, &spoilers).await?;
        write_cache(
            &ctx.library,
            book_id,
            &raw,
            &transcript.locale,
            &self.tunables.extractor_version,
        )
        .await?;

        tracing::info!(
            book_id = book_id.0,
            dna_count = dna.len(),
            spoiler_count = spoilers.len(),
            "fm.dna.extracted"
        );
        Ok(StageOutcome::Done)
    }
}

/// JSON shape we expect from the LLM. The prompt asks for
/// exactly this — defensive parsing tolerates trailing newlines
/// / extra whitespace via `serde_json::from_str`.
#[derive(Debug, Deserialize)]
struct DnaResponse {
    #[serde(default)]
    dna_tags: Vec<String>,
    #[serde(default)]
    spoiler_tags: Vec<String>,
}

/// Segment array (the only thing still in the JSON BLOB after
/// slice B2 — locale moved to its own column).
#[derive(Debug, Deserialize)]
struct CachedTranscript {
    segments: Vec<Segment>,
}

#[derive(Debug, Deserialize)]
struct Segment {
    text: String,
}

/// Flattened view: locale + concatenated segment text.
struct TranscriptView {
    locale: String,
    text: String,
}

async fn load_full_transcript(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<TranscriptView>> {
    let id = book_id.0;
    let full_cache = CacheKey::TranscriptFull.as_str();
    let row = sqlx::query!(
        "SELECT content, locale FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        full_cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("dna load transcript_full: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let Some(locale) = row.locale else {
        return Ok(None);
    };
    let cached: CachedTranscript = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(book_id = id, error = %e, "fm.dna.transcript_unparseable");
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
    Ok(Some(TranscriptView { locale, text }))
}

async fn dna_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    extractor_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let dna_cache = CacheKey::DnaTags.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        dna_cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("dna cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    Ok(row.extractor_version.as_deref() == Some(extractor_version))
}

async fn write_tags(
    library: &LibraryDb,
    book_id: BookId,
    dna_tags: &[String],
    spoiler_tags: &[String],
) -> Result<()> {
    let id = book_id.0;
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("dna tags begin: {e}")))?;

    // Clear prior dna_llm rows for idempotent rewrite.
    sqlx::query!(
        "DELETE FROM book_tags WHERE book_id = ? AND source = ?",
        id,
        TAG_SOURCE_DNA_LLM,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| Error::Database(format!("dna tags clear: {e}")))?;

    for raw in dna_tags {
        let tag = TagKind::Dna.format_tag(&normalise_tag(raw));
        sqlx::query!(
            "INSERT OR IGNORE INTO book_tags (book_id, tag, source) VALUES (?, ?, ?)",
            id,
            tag,
            TAG_SOURCE_DNA_LLM,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("dna tag insert: {e}")))?;
    }
    for raw in spoiler_tags {
        let tag = TagKind::Spoiler.format_tag(&normalise_tag(raw));
        sqlx::query!(
            "INSERT OR IGNORE INTO book_tags (book_id, tag, source) VALUES (?, ?, ?)",
            id,
            tag,
            TAG_SOURCE_DNA_LLM,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("dna spoiler insert: {e}")))?;
    }
    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("dna tags commit: {e}")))?;
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
    let dna_cache = CacheKey::DnaTags.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, extractor_version, locale) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        dna_cache,
        bytes,
        extractor_version,
        locale,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("dna cache write: {e}")))?;
    Ok(())
}

/// Build the prompt sent to the LLM. Public for unit-testing the
/// exact contract.
///
/// `transcript` is the concatenated segment text; we truncate
/// defensively at the head (the LLM has its own context window
/// and a 5-hour book's full transcript is huge — for v0 we send
/// the first ~30k chars which is plenty for thematic signal).
#[must_use]
pub fn build_prompt(transcript: &str, locale: &str, max_dna: usize, max_spoilers: usize) -> String {
    // 30_000 chars ≈ first ~30 minutes of speech at typical
    // audiobook density. Past that, the marginal thematic signal
    // drops while the model's context cost rises.
    const TRANSCRIPT_LIMIT: usize = 30_000;
    let excerpt = if transcript.len() > TRANSCRIPT_LIMIT {
        // Char-boundary-safe truncate.
        let mut end = TRANSCRIPT_LIMIT;
        while end > 0 && !transcript.is_char_boundary(end) {
            end -= 1;
        }
        &transcript[..end]
    } else {
        transcript
    };
    // Schema shape (`dna_tags`, `spoiler_tags` — both arrays of
    // strings) is conveyed to the model by the
    // `complete_structured` bridge with `includeSchemaInPrompt:
    // true`; we don't restate it here. What stays in the prompt
    // is the *content guidance* the schema cannot express:
    // category caps, what each list semantically means, the
    // no-prefix-in-the-string convention, and the
    // English-tags-regardless-of-locale rule.
    format!(
        "You are a metadata extractor for an audiobook library. \
Read the TRANSCRIPT excerpt below and produce two short tag lists.\n\
\n\
- `dna_tags`: at most {max_dna} short, lowercase, hyphenated tags \
describing the book's themes, mood, narrative style, and content \
texture. Tags must be safe to show readers who haven't read the \
book (no plot reveals). Examples: \"cozy\", \"unreliable-narrator\", \
\"slow-burn-romance\", \"morally-grey-cast\". Do NOT include the # \
prefix in the string.\n\
- `spoiler_tags`: at most {max_spoilers} tags marking plot-revealing \
attributes a spoiler-averse reader should not see by default. \
Examples: \"hero-dies\", \"twin-twist\", \"unreliable-narrator-revealed\". \
Only include tags backed by clear evidence in the transcript. Do NOT \
include the ! prefix in the string.\n\
\n\
Write tags in English regardless of TRANSCRIPT language.\n\
\n\
TRANSCRIPT (locale={locale}):\n\
{excerpt}"
    )
}

/// Slug-normalise a tag: lowercase, collapse internal whitespace
/// to a single `-`, drop everything not `[a-z0-9-]`. Leaves
/// hyphens already in the model's output intact.
#[must_use]
pub fn normalise_tag(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_was_dash = false;
    for ch in raw.trim().chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_was_dash = false;
        } else if (c == '-' || c == ' ' || c == '_') && !last_was_dash && !out.is_empty() {
            out.push('-');
            last_was_dash = true;
        }
        // Anything else (punctuation, prefixes the model
        // emitted by mistake, etc.) is silently dropped.
    }
    // Trim trailing dash.
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn clamp(tags: &[String], max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(max.min(tags.len()));
    for t in tags {
        let n = normalise_tag(t);
        if n.is_empty() {
            continue;
        }
        if seen.insert(n.clone()) {
            out.push(n);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// Map a bridge error into a Stage error. The user-facing
/// categories surfaced by `aborg doctor llm` are preserved in
/// the message so debugging is straightforward.
fn bridge_to_stage_error(err: &BridgeError) -> Error {
    Error::stage(STAGE_NAME, format!("Foundation Models: {err}"))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn normalise_tag_lowercases_and_hyphenates() {
        assert_eq!(normalise_tag("Cozy Mystery"), "cozy-mystery");
        assert_eq!(normalise_tag("Found_Family!"), "found-family");
        assert_eq!(normalise_tag("  morally-grey  "), "morally-grey");
        assert_eq!(normalise_tag("--leading---dash--"), "leading-dash");
    }

    #[test]
    fn normalise_tag_drops_non_ascii() {
        // Defensive: non-ASCII text drops to empty; the prompt
        // tells the model to emit English so this is a guard
        // rather than the common path.
        assert_eq!(normalise_tag("zauberhaft"), "zauberhaft");
        assert_eq!(normalise_tag("ünreliable-nårrator"), "nreliable-nrrator");
    }

    #[test]
    fn clamp_dedupes_and_caps() {
        let input = vec![
            "Cozy".to_owned(),
            "cozy".to_owned(),
            "Slow Burn".to_owned(),
            "found-family".to_owned(),
            "morally grey".to_owned(),
        ];
        let out = clamp(&input, 3);
        assert_eq!(out, vec!["cozy", "slow-burn", "found-family"]);
    }

    #[test]
    fn build_prompt_includes_caps_and_locale() {
        let p = build_prompt("Once upon a time…", "en", 5, 2);
        assert!(p.contains("at most 5 short"));
        assert!(p.contains("at most 2 tags"));
        assert!(p.contains("locale=en"));
        assert!(p.contains("Once upon a time"));
    }

    #[test]
    fn build_prompt_truncates_long_transcript() {
        let long = "x".repeat(40_000);
        let p = build_prompt(&long, "en", 5, 2);
        // Header overhead is well under 1k; truncated body
        // should keep total prompt under ~32k chars.
        assert!(p.len() < 32_000, "prompt len was {}", p.len());
    }

    #[test]
    fn parse_dna_response() {
        let json = r#"{"dna_tags":["cozy","slow-burn"],"spoiler_tags":["hero-dies"]}"#;
        let r: DnaResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.dna_tags, vec!["cozy", "slow-burn"]);
        assert_eq!(r.spoiler_tags, vec!["hero-dies"]);
    }

    #[test]
    fn parse_dna_response_tolerates_missing_arrays() {
        // The model occasionally omits an empty array entirely
        // — we should default to Vec::new() rather than erroring.
        let json = r#"{"dna_tags":["cozy"]}"#;
        let r: DnaResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.dna_tags, vec!["cozy"]);
        assert!(r.spoiler_tags.is_empty());
    }

    /// `DNA_SCHEMA_JSON` is the JSON Schema the framework
    /// constrains the model to. Verify it parses as JSON (so the
    /// bridge's schema-parse step won't reject it at runtime) and
    /// names exactly the fields the `DnaResponse` deserialiser
    /// reads. Catches the case where one side adds a field
    /// without the other.
    #[test]
    fn schema_parses_and_matches_response_shape() {
        let v: serde_json::Value = serde_json::from_str(DNA_SCHEMA_JSON).expect("schema parses");
        assert_eq!(v["type"], "object");
        let props = v["properties"]
            .as_object()
            .expect("properties is an object");
        // Both fields the DnaResponse deserialiser reads must
        // be in the schema, and both must be arrays-of-strings.
        for field in ["dna_tags", "spoiler_tags"] {
            let entry = props
                .get(field)
                .unwrap_or_else(|| panic!("schema missing field {field}"));
            assert_eq!(
                entry["type"], "array",
                "{field} must be `type: array` in schema",
            );
            assert_eq!(
                entry["items"]["type"], "string",
                "{field}.items must be `type: string` in schema",
            );
        }
        // `required` must list both keys so the schema enforces
        // them rather than relying on the prompt.
        let required = v["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|x| x.as_str().expect("required entry is string").to_owned())
            .collect::<Vec<_>>();
        assert!(required.contains(&"dna_tags".to_owned()));
        assert!(required.contains(&"spoiler_tags".to_owned()));
    }
}
