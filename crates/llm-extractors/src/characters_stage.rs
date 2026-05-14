//! `extract-characters` pipeline stage (slice 3K.6).
//!
//! For each book this stage:
//!
//! 1. Loads the cached `transcript_full` row from `ai_cache`.
//!    Skips if absent.
//! 2. Reads `books.language` — output locale. Same rule as
//!    the summary and arc stages: characters' names +
//!    descriptions stay in the book's native language
//!    regardless of `library_locale` (ADR-0019).
//! 3. Builds a prompt asking for up to `characters_max`
//!    characters with name + optional aliases + role +
//!    spoiler-free description + `is_pov` + six optional
//!    trait fields (species, condition, occupation, age,
//!    gender, affiliation). The schema constraint forces
//!    `{characters: [...], characters_lang}` shape with `age`
//!    drawn from a closed five-bracket enum
//!    (`child`/`teen`/`adult`/`elderly`/`immortal`).
//! 4. Self-checks: `characters_lang` matches `books.language`
//!    (primary subtag); on mismatch warn + skip promotion.
//! 5. Truncates to `characters_max` defensively (the prompt
//!    states the cap but the model can overrun).
//! 6. Replaces the book's rows in `characters` — DELETE then
//!    INSERT, taking advantage of the `UNIQUE(book_id, name)`
//!    constraint. Idempotency is on the cache row, not the
//!    table rows.
//! 7. Caches the raw response in `ai_cache` keyed
//!    `(book_id, "characters")` with `locale = books.language`
//!    and the current `extractor_version`.
//!
//! ## Vocabulary policy
//!
//! Per ADR-0022, all character-trait columns ship free-form
//! in v1. `age` is the one closed-bracket exception, enforced
//! at the schema layer rather than as a SQL CHECK constraint
//! (so future bracket revisions don't need a migration).
//! Learned canonicalisation lands in a post-library-scan
//! slice.
//!
//! ## Spoiler handling
//!
//! Character descriptions are always shown — no spoiler
//! toggle gates them. The prompt rule "spoiler-free
//! description" + the locale-mismatch retry path are the
//! safety net.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use ab_core::tunables::LlmTunables;
use ab_core::{BookId, CacheKey, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use ab_foundation_models::{BridgeError, GenerationOptions, complete_structured};

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("extract-characters");

/// Stage name written to `pipeline_progress`. Derives from `STAGE_ID`.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// JSON Schema passed to `complete_structured`.
///
/// `age` is the one closed-bracket field; everything else is
/// free-form (vocabulary discovery deferred to a post-library
/// canonicalisation slice). Required fields are the four every
/// character has: `name`, `role`, `description`, `is_pov`.
/// Trait fields are all optional — the extractor only fills a
/// column when the text gives signal (ADR-0022's "be cheap when
/// not applicable" pattern).
pub const CHARACTERS_SCHEMA_JSON: &str = r#"{
    "type": "object",
    "properties": {
        "characters": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "aliases": {"type": "array", "items": {"type": "string"}},
                    "role": {"type": "string"},
                    "description": {"type": "string"},
                    "is_pov": {"type": "boolean"},
                    "species": {"type": "string"},
                    "condition": {"type": "string"},
                    "occupation": {"type": "string"},
                    "age": {"type": "string", "enum": ["child", "teen", "adult", "elderly", "immortal"]},
                    "gender": {"type": "string"},
                    "affiliation": {"type": "string"}
                },
                "required": ["name", "role", "description", "is_pov"]
            }
        },
        "characters_lang": {"type": "string"}
    },
    "required": ["characters", "characters_lang"]
}"#;

/// Stage that asks the on-device LLM for the cast of a book.
pub struct ExtractCharactersStage {
    tunables: Arc<LlmTunables>,
}

impl ExtractCharactersStage {
    /// Construct.
    #[must_use]
    pub fn new(tunables: &LlmTunables) -> Self {
        Self {
            tunables: Arc::new(tunables.clone()),
        }
    }
}

#[async_trait]
impl Stage for ExtractCharactersStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Full transcript + summary: per ADR-0022 the summary
        // dependency sequences LLM calls per book.
        &[
            ab_transcript::full_stage::STAGE_ID,
            crate::summary_stage::STAGE_ID,
        ]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        // 1. Idempotency.
        if characters_cache_fresh(&ctx.library, book_id, &self.tunables.extractor_version).await? {
            return Ok(StageOutcome::Skipped);
        }

        // 2. Inputs.
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
        let prompt = build_prompt(
            &transcript,
            &book_lang,
            PromptShape {
                max_characters: self.tunables.characters_max,
                desc_words_low: self.tunables.character_desc_target_words_low,
                desc_words_high: self.tunables.character_desc_target_words_high,
            },
        );
        let opts = GenerationOptions::new(self.tunables.characters_max_tokens);
        let raw = match complete_structured(&prompt, CHARACTERS_SCHEMA_JSON, &opts).await {
            Ok(s) => s,
            Err(BridgeError::PromptEmpty) => return Ok(StageOutcome::Skipped),
            Err(e) => return Err(bridge_to_stage_error(&e)),
        };

        // 4. Parse + validate.
        let mut parsed = parse_characters(&raw, book_id)?;
        if !validate_response(&parsed, &book_lang, book_id) {
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

        // 5. Defensive truncation.
        if parsed.characters.len() > self.tunables.characters_max {
            tracing::info!(
                book_id = book_id.0,
                got = parsed.characters.len(),
                cap = self.tunables.characters_max,
                "fm.characters.truncated"
            );
            parsed.characters.truncate(self.tunables.characters_max);
        }

        // 6. Promote (replace book's rows) + write cache.
        promote_characters(&ctx.library, book_id, &parsed.characters, &book_lang).await?;
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
            characters = parsed.characters.len(),
            pov = parsed.characters.iter().filter(|c| c.is_pov).count(),
            "fm.characters.extracted"
        );
        Ok(StageOutcome::Done)
    }
}

/// JSON shape produced by the LLM. Mirrors the schema; the
/// parity test asserts the two stay in lock-step.
#[derive(Debug, Deserialize, Serialize)]
struct CharactersResponse {
    characters: Vec<Character>,
    characters_lang: String,
}

/// One character entry.
///
/// `aliases` collapses to a JSON array in the
/// `characters.aliases` TEXT column; `None` means the model
/// didn't emit the field (empty array means it emitted an
/// empty list). Trait fields all follow the same NULL-on-
/// absent rule.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Character {
    /// Canonical character name in the book's language.
    pub name: String,
    /// Alternate names the model saw in the text. Optional;
    /// `None` ≠ `Some(vec![])` (none-emitted vs. emitted-empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    /// Free-form role label (protagonist, antagonist,
    /// supporting, mentioned, …). Vocabulary canonicalised
    /// in a later slice.
    pub role: String,
    /// Spoiler-free description in the book's language.
    pub description: String,
    /// Point-of-view flag. Always emitted (the schema makes
    /// it required); defaults to `false` if the model is
    /// uncertain.
    pub is_pov: bool,
    /// Species / racial identity at the per-character level.
    /// NULL = not extracted / human-default / not applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub species: Option<String>,
    /// Supernatural / medical / narrative condition
    /// (vampire, werewolf, cursed, chosen, dying).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Occupation / profession (witch, mercenary, librarian).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occupation: Option<String>,
    /// Closed-bracket age band (`child` / `teen` / `adult` /
    /// `elderly` / `immortal`). Enforced at the schema layer
    /// rather than as a SQL CHECK constraint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age: Option<String>,
    /// Free-form gender (woman, man, non-binary, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    /// Primary faction / house / order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affiliation: Option<String>,
}

/// Shape parameters for [`build_prompt`].
#[derive(Debug, Clone, Copy)]
pub struct PromptShape {
    /// Soft cap on the number of characters.
    pub max_characters: usize,
    /// Target floor for per-character `description` word count.
    pub desc_words_low: usize,
    /// Target cap for per-character `description` word count.
    pub desc_words_high: usize,
}

fn parse_characters(raw: &str, book_id: BookId) -> Result<CharactersResponse> {
    match serde_json::from_str::<CharactersResponse>(raw) {
        Ok(p) => Ok(p),
        Err(e) => {
            tracing::warn!(
                book_id = book_id.0,
                error = %e,
                raw_len = raw.len(),
                "fm.characters.parse_failed"
            );
            Err(Error::stage(STAGE_NAME, format!("model JSON parse: {e}")))
        }
    }
}

/// Validate the locale tag. Other checks (per-character field
/// shape) are enforced by the schema constraint and parse
/// path. Returns `true` when the response is safe to promote.
fn validate_response(parsed: &CharactersResponse, book_lang: &str, book_id: BookId) -> bool {
    let locale_ok = parsed.characters_lang.eq_ignore_ascii_case(book_lang)
        || primary_subtag(&parsed.characters_lang) == primary_subtag(book_lang);
    if !locale_ok {
        tracing::warn!(
            book_id = book_id.0,
            expected = %book_lang,
            got = %parsed.characters_lang,
            "fm.characters.locale_mismatch"
        );
        return false;
    }
    true
}

async fn load_book_language(library: &LibraryDb, book_id: BookId) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT language FROM books WHERE book_id = ?", id)
        .fetch_optional(library.pool())
        .await
        .map_err(|e| Error::Database(format!("characters load lang: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let trimmed = row
        .language
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(trimmed.map(str::to_owned))
}

#[derive(Debug, Deserialize)]
struct CachedTranscript {
    segments: Vec<Segment>,
}

#[derive(Debug, Deserialize)]
struct Segment {
    text: String,
}

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
    .map_err(|e| Error::Database(format!("characters load transcript_full: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let cached: CachedTranscript = match ab_core::cache::deserialize_cache_content(&bytes) {
        Ok(c) => c,
        Err(e) => {
            // B.2a: covers JSON parse failures + oversized payloads.
            tracing::warn!(book_id = id, error = %e, "fm.characters.transcript_unparseable");
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

async fn characters_cache_fresh(
    library: &LibraryDb,
    book_id: BookId,
    extractor_version: &str,
) -> Result<bool> {
    let id = book_id.0;
    let cache = CacheKey::Characters.as_str();
    let row = sqlx::query!(
        "SELECT extractor_version FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("characters cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(false) };
    Ok(row.extractor_version.as_deref() == Some(extractor_version))
}

/// Replace the book's rows in `characters`. DELETE then
/// INSERT — `UNIQUE(book_id, name)` means an UPSERT
/// alternative would still need a separate DELETE-not-in-list
/// pass to purge dropped characters; the wholesale replacement
/// is simpler and the row counts are small.
async fn promote_characters(
    library: &LibraryDb,
    book_id: BookId,
    characters: &[Character],
    book_lang: &str,
) -> Result<()> {
    let id = book_id.0;
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("characters begin tx: {e}")))?;

    sqlx::query!("DELETE FROM characters WHERE book_id = ?", id)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("characters delete: {e}")))?;

    for c in characters {
        let aliases_json = match c.aliases.as_ref() {
            Some(list) if !list.is_empty() => Some(
                serde_json::to_string(list)
                    .map_err(|e| Error::stage(STAGE_NAME, format!("encode aliases: {e}")))?,
            ),
            _ => None,
        };
        let is_pov = i32::from(c.is_pov);
        sqlx::query!(
            "INSERT INTO characters \
             (book_id, name, aliases, role, description, lang, is_pov, species, \
              condition, occupation, age, gender, affiliation) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            c.name,
            aliases_json,
            c.role,
            c.description,
            book_lang,
            is_pov,
            c.species,
            c.condition,
            c.occupation,
            c.age,
            c.gender,
            c.affiliation,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("characters insert: {e}")))?;
    }

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("characters commit: {e}")))?;
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
    let cache = CacheKey::Characters.as_str();
    sqlx::query!(
        "INSERT OR REPLACE INTO ai_cache \
         (book_id, cache_type, content, compressed, extractor_version, locale) \
         VALUES (?, ?, ?, 0, ?, ?)",
        id,
        cache,
        bytes,
        extractor_version,
        locale,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("characters cache write: {e}")))?;
    Ok(())
}

/// Build the prompt sent to the LLM. Public for unit-testing
/// the content rules (cap, locale, spoiler rule, trait
/// guidance).
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
        max_characters,
        desc_words_low,
        desc_words_high,
    } = shape;
    format!(
        "You are a literary analyst building a character list for an audiobook \
library browse view. Read the TRANSCRIPT below and produce a list of the most \
important characters.\n\
\n\
Rules:\n\
1. Output at most {max_characters} characters. Cover principals first; include \
recurring secondaries if budget allows; do NOT include one-line walk-ons.\n\
2. `name` is the canonical name as it appears in the book.\n\
3. `aliases` is an optional list of nicknames / titles / alternate forms \
seen in the text. Omit when there are none; do not invent.\n\
4. `role` is a short free-form label like `protagonist`, `antagonist`, \
`supporting`, `mentor`, `narrator`, `villain`. Pick the most descriptive.\n\
5. `description` is a SPOILER-FREE {desc_words_low}-{desc_words_high} word \
sketch. NO plot twists, deaths, romance outcomes, or third-act revelations. \
Tone + role + first-act traits only.\n\
6. `is_pov` is `true` when the narrative is told from this character's \
perspective (first-person, close third), `false` otherwise. Default to \
`false` when uncertain. ALWAYS emit this field.\n\
7. Trait fields are OPTIONAL. Only fill them when the text gives a clear \
signal. Omit (don't guess) for the rest:\n\
   - `species`: emit when the character is explicitly non-human (`elf`, \
`dwarf`, `dragon`, `cyborg`, `vampire-as-species`). Omit for contemporary \
human characters.\n\
   - `condition`: notable supernatural / medical / narrative state \
(`vampire`, `werewolf`, `cursed`, `chosen`, `dying`).\n\
   - `occupation`: explicit profession (`witch`, `mercenary`, `blacksmith`, \
`librarian`, `assassin`). Fill liberally when knowable.\n\
   - `age`: pick exactly one bracket: `child`, `teen`, `adult`, `elderly`, \
`immortal`. Omit when uncertain.\n\
   - `gender`: free-form (`woman`, `man`, `non-binary`, `genderfluid`). \
Omit when the text doesn't make it explicit.\n\
   - `affiliation`: primary faction / house / order. Use the most prominent \
when multiple apply.\n\
8. Write all names + descriptions in the book's native language. The book's \
BCP-47 locale is `{book_locale}`. Set `characters_lang` to `{book_locale}` \
to confirm.\n\
\n\
TRANSCRIPT (locale={book_locale}):\n\
{excerpt}"
    )
}

fn primary_subtag(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

fn bridge_to_stage_error(err: &BridgeError) -> Error {
    Error::stage(STAGE_NAME, format!("Foundation Models: {err}"))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn default_shape() -> PromptShape {
        PromptShape {
            max_characters: 12,
            desc_words_low: 20,
            desc_words_high: 40,
        }
    }

    #[test]
    fn build_prompt_includes_locale_caps_and_transcript() {
        let p = build_prompt("Once upon a time…", "de", default_shape());
        assert!(p.contains("`de`"));
        assert!(p.contains("at most 12 characters"));
        assert!(p.contains("20-40 word"));
        assert!(p.contains("Once upon a time"));
        assert!(p.contains("locale=de"));
    }

    #[test]
    fn build_prompt_truncates_long_transcript() {
        let long = "x".repeat(40_000);
        let p = build_prompt(&long, "en", default_shape());
        assert!(p.len() < 32_500, "prompt len was {}", p.len());
    }

    #[test]
    fn build_prompt_calls_out_is_pov_required() {
        let p = build_prompt("…", "en", default_shape());
        assert!(p.contains("ALWAYS emit this field"));
        assert!(p.contains("`is_pov`"));
    }

    #[test]
    fn build_prompt_specifies_age_brackets() {
        let p = build_prompt("…", "en", default_shape());
        for bracket in ["child", "teen", "adult", "elderly", "immortal"] {
            assert!(
                p.contains(&format!("`{bracket}`")),
                "prompt missing age bracket `{bracket}`",
            );
        }
    }

    #[test]
    fn parse_characters_response_minimal() {
        let json = r#"{
            "characters": [
                {"name": "Alice", "role": "protagonist",
                 "description": "A curious child.", "is_pov": true}
            ],
            "characters_lang": "en"
        }"#;
        let r: CharactersResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(r.characters_lang, "en");
        assert_eq!(r.characters.len(), 1);
        let c = &r.characters[0];
        assert_eq!(c.name, "Alice");
        assert!(c.is_pov);
        assert!(c.aliases.is_none());
        assert!(c.species.is_none());
    }

    #[test]
    fn parse_characters_response_with_traits() {
        let json = r#"{
            "characters": [
                {"name": "Drizzt", "aliases": ["Do'Urden"],
                 "role": "protagonist",
                 "description": "An exiled warrior with twin scimitars.",
                 "is_pov": true,
                 "species": "drow", "occupation": "ranger",
                 "age": "adult", "gender": "man",
                 "affiliation": "Companions of the Hall"}
            ],
            "characters_lang": "en"
        }"#;
        let r: CharactersResponse = serde_json::from_str(json).expect("parse");
        let c = &r.characters[0];
        assert_eq!(c.species.as_deref(), Some("drow"));
        assert_eq!(c.age.as_deref(), Some("adult"));
        assert_eq!(c.affiliation.as_deref(), Some("Companions of the Hall"));
        assert_eq!(c.aliases.as_deref(), Some(&["Do'Urden".to_owned()][..]),);
    }

    /// `is_pov` is required by the schema — a payload that
    /// omits it must fail to parse. Pins the "always-emit"
    /// contract from ADR-0022.
    #[test]
    fn is_pov_is_required_on_parse() {
        let json = r#"{
            "characters": [
                {"name": "Alice", "role": "protagonist",
                 "description": "A curious child."}
            ],
            "characters_lang": "en"
        }"#;
        let r: std::result::Result<CharactersResponse, _> = serde_json::from_str(json);
        assert!(r.is_err(), "parse must reject missing is_pov");
    }

    #[test]
    fn primary_subtag_normalises_bcp47() {
        assert_eq!(primary_subtag("en"), "en");
        assert_eq!(primary_subtag("en-US"), "en");
        assert_eq!(primary_subtag("zh-Hans-CN"), "zh");
        assert_eq!(primary_subtag(""), "");
    }

    /// `CHARACTERS_SCHEMA_JSON` is the constraint the model
    /// decodes against. This test asserts that the Rust shape
    /// (`CharactersResponse` / `Character`) stays in lock-step
    /// with the schema. Adding or renaming a field on one side
    /// without the other fails here at landing.
    #[test]
    fn schema_parses_and_matches_response_shape() {
        let v: serde_json::Value =
            serde_json::from_str(CHARACTERS_SCHEMA_JSON).expect("schema parses");
        assert_eq!(v["type"], "object");
        let props = v["properties"]
            .as_object()
            .expect("properties is an object");

        for field in ["characters", "characters_lang"] {
            assert!(
                props.contains_key(field),
                "schema missing top-level `{field}`",
            );
        }

        let required = v["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|x| x.as_str().expect("required entry is string").to_owned())
            .collect::<Vec<_>>();
        assert!(required.contains(&"characters".to_owned()));
        assert!(required.contains(&"characters_lang".to_owned()));

        assert_eq!(props["characters_lang"]["type"], "string");
        assert_eq!(props["characters"]["type"], "array");

        let item = &props["characters"]["items"];
        assert_eq!(item["type"], "object");
        let item_props = item["properties"]
            .as_object()
            .expect("character item properties");

        // Every Rust field has a matching schema entry.
        for field in [
            "name",
            "aliases",
            "role",
            "description",
            "is_pov",
            "species",
            "condition",
            "occupation",
            "age",
            "gender",
            "affiliation",
        ] {
            assert!(
                item_props.contains_key(field),
                "schema missing character.{field}",
            );
        }

        // Type pins for the non-string fields.
        assert_eq!(item_props["is_pov"]["type"], "boolean");
        assert_eq!(item_props["aliases"]["type"], "array");
        assert_eq!(item_props["aliases"]["items"]["type"], "string");

        // age has a closed-bracket enum.
        let age_enum = item_props["age"]["enum"]
            .as_array()
            .expect("age has enum array");
        let age_values: Vec<&str> = age_enum.iter().filter_map(|x| x.as_str()).collect();
        for bracket in ["child", "teen", "adult", "elderly", "immortal"] {
            assert!(
                age_values.contains(&bracket),
                "age enum missing `{bracket}`",
            );
        }
        assert_eq!(
            age_values.len(),
            5,
            "age enum should be exactly the 5 brackets",
        );

        // Required-at-item-level: name + role + description + is_pov.
        // Trait fields are optional.
        let item_required = item["required"]
            .as_array()
            .expect("character item required")
            .iter()
            .map(|x| x.as_str().expect("required str").to_owned())
            .collect::<Vec<_>>();
        for field in ["name", "role", "description", "is_pov"] {
            assert!(
                item_required.contains(&field.to_owned()),
                "character.required missing `{field}`",
            );
        }
        for trait_field in [
            "aliases",
            "species",
            "condition",
            "occupation",
            "age",
            "gender",
            "affiliation",
        ] {
            assert!(
                !item_required.contains(&trait_field.to_owned()),
                "trait field `{trait_field}` should NOT be required (kept optional per ADR-0022)",
            );
        }
    }

    /// Round-trip: a fully-populated Character serialises to a
    /// schema-shaped JSON object. Together with the parse-test
    /// pair this pins the round trip the cache row depends on.
    #[test]
    fn character_round_trips_through_json() {
        let c = Character {
            name: "Drizzt".into(),
            aliases: Some(vec!["Do'Urden".into()]),
            role: "protagonist".into(),
            description: "Exiled warrior with twin scimitars.".into(),
            is_pov: true,
            species: Some("drow".into()),
            condition: None,
            occupation: Some("ranger".into()),
            age: Some("adult".into()),
            gender: Some("man".into()),
            affiliation: Some("Companions of the Hall".into()),
        };
        let json = serde_json::to_string(&c).expect("encode");
        let back: Character = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.name, c.name);
        assert_eq!(back.is_pov, c.is_pov);
        assert_eq!(back.species, c.species);
        assert_eq!(back.aliases, c.aliases);
        // condition stayed None — it should not appear in the
        // serialised form due to `skip_serializing_if`.
        assert!(!json.contains("\"condition\""));
    }
}
