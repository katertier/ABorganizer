//! Typed identifier for `ai_cache.cache_type`.
//!
//! Every cache producer (transcribe stages, LLM extractor
//! stages) writes one of a closed set of values. Keeping that
//! set in one place — and using it through a typed enum
//! instead of inline `"transcript_head"` literals — catches the
//! "stage typoed its cache key" class of bug at compile time.
//! With strings, a producer can write `"transcribe_head"` (note
//! the extra 'e'), the freshness-check still reads
//! `"transcript_head"`, and the stage re-runs every scheduler
//! tick forever. The enum makes that impossible.
//!
//! Add a new cache type: add a variant here, point its
//! `as_str()` arm at the chosen string, and update any
//! freshness checks. The enum is the single source of truth.

use serde::{Deserialize, Serialize};

/// One row of `ai_cache.cache_type`.
///
/// Variants enumerate every cache producer the workspace
/// currently knows about. The `Display` + `AsRef<str>` impls
/// produce the string that lives in the DB column — bind these
/// directly into `sqlx::query!` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKey {
    /// First-N-seconds transcript (default 360 s). Written by
    /// the `transcribe-head-tail` stage; consumed by the
    /// language-detect post-pass, the transcript extractor
    /// stage, and downstream LLM extractors.
    TranscriptHead,
    /// Last-N-seconds transcript (default 30 s). Written by
    /// the `transcribe-head-tail` stage; consumed by the
    /// outro audiologo + last-sentence-boundary work.
    TranscriptTail,
    /// Three short windows at 25 / 50 / 75 % of book duration.
    /// Written by `transcribe-samples`; primary source of
    /// post-transcribe language confirmation.
    TranscriptSamples,
    /// Whole-book transcript, concatenated across files.
    /// Written by `transcribe-full` (Idle priority); consumed
    /// by every LLM extractor that wants context beyond the
    /// first 6 minutes.
    TranscriptFull,
    /// Raw LLM response for the DNA-tag extractor. Promoted to
    /// `#`-prefixed and `!`-prefixed rows in `book_tags`.
    DnaTags,
    /// Raw LLM response for the spoiler-free summary
    /// extractor. Promoted to `books.summary_spoiler_free` +
    /// `books.summary_spoiler_free_lang`.
    SummarySpoilerFree,
    /// Raw LLM response for the story-arc extractor. Promoted
    /// to `books.story_arc_json` (JSON array of step records).
    StoryArc,
    /// Raw LLM response for the character extractor. Promoted
    /// to rows in the `characters` table.
    Characters,
    /// Raw LLM response for the setting extractor. Promoted
    /// to `books.setting` + `books.setting_lang` (paragraph)
    /// and `book_tags` rows with `source='setting_llm'`
    /// (`$`-prefixed tags across 10 categories per ADR-0022).
    Setting,
    /// Proper-noun dictionary extracted from a paired EPUB
    /// companion (ADR-0043 § C.4). Consumed by the C.5
    /// `transcript-correct-via-epub` stage to fix near-miss
    /// proper-noun spellings in `transcript_full`. Payload is
    /// JSON `{ "entries": [...], "language": "..." }`.
    EpubNameDict,
}

impl CacheKey {
    /// The exact string written into `ai_cache.cache_type`.
    /// Use this for `sqlx::query!` bind params and for any
    /// API-level introspection (`aborg doctor` etc.).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TranscriptHead => "transcript_head",
            Self::TranscriptTail => "transcript_tail",
            Self::TranscriptSamples => "transcript_samples",
            Self::TranscriptFull => "transcript_full",
            Self::DnaTags => "dna_tags",
            Self::SummarySpoilerFree => "summary_spoiler_free",
            Self::StoryArc => "story_arc",
            Self::Characters => "characters",
            Self::Setting => "setting",
            Self::EpubNameDict => "epub_name_dict",
        }
    }

    /// Parse the `cache_type` column back into the typed enum.
    /// Returns `None` for unknown strings — callers can treat
    /// those as legacy / not-our-key. Named `parse` (not
    /// `from_str`) to avoid colliding with the `FromStr` trait
    /// method, which would force an `Err` type we don't want
    /// for an "unknown is fine" lookup.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "transcript_head" => Some(Self::TranscriptHead),
            "transcript_tail" => Some(Self::TranscriptTail),
            "transcript_samples" => Some(Self::TranscriptSamples),
            "transcript_full" => Some(Self::TranscriptFull),
            "dna_tags" => Some(Self::DnaTags),
            "summary_spoiler_free" => Some(Self::SummarySpoilerFree),
            "story_arc" => Some(Self::StoryArc),
            "characters" => Some(Self::Characters),
            "setting" => Some(Self::Setting),
            "epub_name_dict" => Some(Self::EpubNameDict),
            _ => None,
        }
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for CacheKey {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Error returned by `<CacheKey as FromStr>::from_str` when the
/// input doesn't match any [`CacheKey`] variant. Carries the
/// offending string for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCacheKeyError(pub String);

impl std::fmt::Display for ParseCacheKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown ai_cache.cache_type value: {:?}", self.0)
    }
}

impl std::error::Error for ParseCacheKeyError {}

impl std::str::FromStr for CacheKey {
    type Err = ParseCacheKeyError;

    /// Parse an `ai_cache.cache_type` string into the typed enum.
    /// Returns [`ParseCacheKeyError`] if the string isn't one of
    /// the known variants — distinct from [`CacheKey::parse`]
    /// which returns `Option<Self>` for the "unknown is fine"
    /// lookup path.
    ///
    /// Use `from_str` when the caller treats an unknown string as
    /// a user-visible error (REPL / admin tool / API deserialise);
    /// use `parse` when the caller wants to silently skip unknowns
    /// (e.g. legacy DB rows during a migration).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| ParseCacheKeyError(s.to_owned()))
    }
}

/// Map a stage's name (as it appears in `pipeline_progress.stage`
/// and `StageId::as_str()`) to the `ai_cache` rows the stage
/// produces.
///
/// Used by the `aborg book retry` endpoint (ADR-0023) — the
/// daemon clears all returned [`CacheKey`] rows alongside the
/// `pipeline_progress` row so the stage re-runs from a clean
/// slate. Stages that produce no `ai_cache` rows (scan,
/// fingerprint, audnexus-*, identity-*, consensus, chapter-*,
/// detect-description-lang, run-transcript-extractors,
/// extract-summary-spoiler-free-series) return `&[]`.
///
/// Unknown stage names return `None` so the caller can
/// distinguish "stage exists but has no caches" from "stage
/// not registered." The DAG is the authoritative
/// known-stages list; this lookup is purely cache-side.
///
/// Adding a new stage with cache output: extend the match
/// arm. The schema-parity tests on each extractor guard
/// against the cache-key string itself drifting.
#[must_use]
pub fn cache_keys_for_stage(stage: &str) -> Option<&'static [CacheKey]> {
    use CacheKey::{
        Characters, DnaTags, EpubNameDict, Setting, StoryArc, SummarySpoilerFree, TranscriptFull,
        TranscriptHead, TranscriptSamples, TranscriptTail,
    };
    // `&'static` literals so the result is cheap to return.
    const HEAD_TAIL: &[CacheKey] = &[TranscriptHead, TranscriptTail];
    const SAMPLES: &[CacheKey] = &[TranscriptSamples];
    const FULL: &[CacheKey] = &[TranscriptFull];
    const DNA: &[CacheKey] = &[DnaTags];
    const SUMMARY: &[CacheKey] = &[SummarySpoilerFree];
    const ARC: &[CacheKey] = &[StoryArc];
    const CHARS: &[CacheKey] = &[Characters];
    const SETTING: &[CacheKey] = &[Setting];
    const EPUB_NAME_DICT: &[CacheKey] = &[EpubNameDict];
    const NONE: &[CacheKey] = &[];

    Some(match stage {
        "transcribe-head-tail" => HEAD_TAIL,
        "transcribe-samples" => SAMPLES,
        "transcribe-full" => FULL,
        "extract-dna-tags" => DNA,
        "extract-summary-spoiler-free" => SUMMARY,
        "extract-story-arc" => ARC,
        "extract-characters" => CHARS,
        "extract-setting" => SETTING,
        "extract-epub-name-dict" => EPUB_NAME_DICT,
        // Stages without ai_cache output. They still have
        // pipeline_progress rows the retry endpoint clears,
        // but no cache-side cleanup.
        "tag-read"
        | "fingerprint"
        | "audible-search"
        | "audnexus-enrich"
        | "audnexus-chapters"
        | "consensus"
        | "identity-resolve"
        | "embedded-chapters"
        | "chapter-pick-winner"
        | "detect-description-lang"
        | "run-transcript-extractors"
        | "extract-summary-spoiler-free-series" => NONE,
        _ => return None,
    })
}

/// Hard cap on the size of any `ai_cache.content` payload before
/// `serde_json::from_slice` is invoked. Slice B.2a (tracker #114).
///
/// Three motivations:
///
/// 1. **Defense-in-depth.** A malformed or maliciously-expanded
///    cache row that exceeds normal extractor output should not
///    feed an uncapped deserialiser — `serde_json` will dutifully
///    parse a 4 GB JSON value into a multi-gigabyte tree and
///    OOM the daemon.
/// 2. **Bug detector.** Production payloads are well under 1 MB
///    (full transcripts top out at a few hundred kB; LLM
///    extractor outputs at a few kB). A cache row past this cap
///    is almost certainly the result of a producer bug or
///    schema-version mismatch, not legitimate growth.
/// 3. **Predictable latency.** Bounding payload size bounds
///    deserialisation time, which feeds the rest of the pipeline.
///
/// 32 `MiB` is comfortably above legitimate growth (~30× current
/// max observed) and well below memory-pressure thresholds.
/// Operators with unusual workloads can revisit this constant in
/// a future tunable, but no production data has come close so
/// far.
pub const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Error returned by [`deserialize_cache_content`] when the input
/// exceeds [`MAX_CACHE_BYTES`] or fails to parse as JSON. The
/// `oversized` variant carries the actual size so the caller can
/// log usefully.
#[derive(Debug)]
pub enum CacheDeserializeError {
    /// Payload size exceeded [`MAX_CACHE_BYTES`].
    Oversized {
        /// Actual byte length received.
        actual: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// `serde_json` failed to parse the payload.
    Json(serde_json::Error),
}

impl std::fmt::Display for CacheDeserializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversized { actual, cap } => write!(
                f,
                "ai_cache.content payload {actual} bytes exceeds cap {cap}"
            ),
            Self::Json(e) => write!(f, "ai_cache.content JSON parse failed: {e}"),
        }
    }
}

impl std::error::Error for CacheDeserializeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(e) => Some(e),
            Self::Oversized { .. } => None,
        }
    }
}

impl From<serde_json::Error> for CacheDeserializeError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// Deserialise an `ai_cache.content` payload into `T`, enforcing
/// the [`MAX_CACHE_BYTES`] size cap first.
///
/// Every production reader of `ai_cache.content` must route through
/// this helper instead of calling `serde_json::from_slice`
/// directly. On size cap exceedance, returns
/// [`CacheDeserializeError::Oversized`] without invoking the
/// deserialiser — the caller logs + falls back to "no cache"
/// semantics (re-run the producing stage).
///
/// # Errors
///
/// - [`CacheDeserializeError::Oversized`] if `bytes.len() >
///   MAX_CACHE_BYTES`.
/// - [`CacheDeserializeError::Json`] if the bytes are within the
///   cap but fail to parse as `T`.
///
/// # Examples
///
/// ```
/// # use ab_core::cache::deserialize_cache_content;
/// # use serde::Deserialize;
/// #[derive(Deserialize)]
/// struct Payload { items: Vec<String> }
///
/// let bytes = br#"{"items":["a","b"]}"#;
/// let p: Payload = deserialize_cache_content(bytes).unwrap();
/// assert_eq!(p.items.len(), 2);
/// ```
pub fn deserialize_cache_content<T>(bytes: &[u8]) -> Result<T, CacheDeserializeError>
where
    T: serde::de::DeserializeOwned,
{
    if bytes.len() > MAX_CACHE_BYTES {
        return Err(CacheDeserializeError::Oversized {
            actual: bytes.len(),
            cap: MAX_CACHE_BYTES,
        });
    }
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_variant() {
        // Pinning the closed set: when a new variant lands,
        // this test fails until the round-trip is added.
        for key in [
            CacheKey::TranscriptHead,
            CacheKey::TranscriptTail,
            CacheKey::TranscriptSamples,
            CacheKey::TranscriptFull,
            CacheKey::DnaTags,
            CacheKey::SummarySpoilerFree,
            CacheKey::StoryArc,
            CacheKey::Characters,
            CacheKey::Setting,
            CacheKey::EpubNameDict,
        ] {
            let s = key.as_str();
            assert_eq!(CacheKey::parse(s), Some(key), "round-trip {s}");
        }
    }

    #[test]
    fn unknown_strings_return_none() {
        assert_eq!(CacheKey::parse(""), None);
        assert_eq!(CacheKey::parse("transcribe_head"), None); // common typo
        assert_eq!(CacheKey::parse("TRANSCRIPT_HEAD"), None); // case-sensitive
    }

    #[test]
    fn display_matches_as_str() {
        let key = CacheKey::DnaTags;
        assert_eq!(format!("{key}"), "dna_tags");
    }

    #[test]
    fn from_str_round_trips_every_variant() {
        for key in [
            CacheKey::TranscriptHead,
            CacheKey::TranscriptTail,
            CacheKey::TranscriptSamples,
            CacheKey::TranscriptFull,
            CacheKey::DnaTags,
            CacheKey::SummarySpoilerFree,
            CacheKey::StoryArc,
            CacheKey::Characters,
            CacheKey::Setting,
            CacheKey::EpubNameDict,
        ] {
            let parsed: CacheKey = key.as_str().parse().expect("from_str round trip");
            assert_eq!(parsed, key);
        }
    }

    #[test]
    fn from_str_rejects_unknown_with_diagnostic() {
        let err = "transcribe_head".parse::<CacheKey>().unwrap_err();
        assert_eq!(err.0, "transcribe_head");
        let msg = format!("{err}");
        assert!(msg.contains("transcribe_head"), "got: {msg}");
    }

    // ── cache_keys_for_stage (ADR-0023) ─────────────────────────

    #[test]
    fn stage_lookup_extract_stages_produce_their_cache_keys() {
        // Each LLM extractor produces exactly one ai_cache row.
        // The retry endpoint clears whatever this list returns.
        for (stage, expected) in [
            ("extract-dna-tags", &[CacheKey::DnaTags][..]),
            (
                "extract-summary-spoiler-free",
                &[CacheKey::SummarySpoilerFree][..],
            ),
            ("extract-story-arc", &[CacheKey::StoryArc][..]),
            ("extract-characters", &[CacheKey::Characters][..]),
            ("extract-setting", &[CacheKey::Setting][..]),
            ("extract-epub-name-dict", &[CacheKey::EpubNameDict][..]),
        ] {
            assert_eq!(
                cache_keys_for_stage(stage),
                Some(expected),
                "lookup for `{stage}`",
            );
        }
    }

    #[test]
    fn stage_lookup_transcribe_head_tail_returns_both_caches() {
        // head-tail is the one multi-output stage. The retry
        // endpoint deletes BOTH rows; missing one would leave
        // a stale tail row pointing at the old extractor_version.
        let keys = cache_keys_for_stage("transcribe-head-tail").expect("known stage");
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&CacheKey::TranscriptHead));
        assert!(keys.contains(&CacheKey::TranscriptTail));
    }

    #[test]
    fn stage_lookup_non_cache_stages_return_empty_slice() {
        // Cache-less stages must not return None — None means
        // "stage not registered". An empty slice is the right
        // signal for "registered, but no caches to clear".
        for stage in [
            "tag-read",
            "fingerprint",
            "audnexus-enrich",
            "consensus",
            "identity-resolve",
            "chapter-pick-winner",
            "detect-description-lang",
            "run-transcript-extractors",
            "extract-summary-spoiler-free-series",
        ] {
            let keys = cache_keys_for_stage(stage)
                .unwrap_or_else(|| panic!("`{stage}` should be a known no-cache stage"));
            assert!(
                keys.is_empty(),
                "`{stage}` claims to produce {keys:?} but is a no-cache stage",
            );
        }
    }

    #[test]
    fn stage_lookup_unknown_stage_returns_none() {
        assert!(cache_keys_for_stage("not-a-real-stage").is_none());
        assert!(cache_keys_for_stage("").is_none());
        assert!(cache_keys_for_stage("EXTRACT-DNA-TAGS").is_none()); // case-sensitive
    }

    // ---- B.2a — MAX_CACHE_BYTES + deserialize_cache_content ----

    #[derive(serde::Deserialize, Debug, PartialEq, Eq)]
    struct SmallPayload {
        items: Vec<String>,
    }

    #[test]
    fn deserialize_cache_content_happy_path() {
        let bytes = br#"{"items":["a","b","c"]}"#;
        let p: SmallPayload = deserialize_cache_content(bytes).expect("parse");
        assert_eq!(
            p,
            SmallPayload {
                items: vec!["a".into(), "b".into(), "c".into()]
            }
        );
    }

    #[test]
    fn deserialize_cache_content_rejects_oversized_without_parsing() {
        // Payload of (MAX_CACHE_BYTES + 1) bytes is rejected
        // BEFORE serde_json runs — verifies the cap is the
        // first gate.
        let oversized = vec![b' '; MAX_CACHE_BYTES + 1];
        let err = deserialize_cache_content::<serde_json::Value>(&oversized)
            .expect_err("must reject oversized payload");
        match err {
            CacheDeserializeError::Oversized { actual, cap } => {
                assert_eq!(actual, MAX_CACHE_BYTES + 1);
                assert_eq!(cap, MAX_CACHE_BYTES);
            }
            CacheDeserializeError::Json(_) => panic!("must reject by size, not parse"),
        }
    }

    #[test]
    fn deserialize_cache_content_allows_at_exact_cap() {
        // A payload exactly at MAX_CACHE_BYTES is allowed past
        // the size gate (the cap is `>`, not `>=`); whether it
        // parses is a separate matter.
        let mut bytes = Vec::with_capacity(MAX_CACHE_BYTES);
        bytes.push(b'"');
        bytes.resize(MAX_CACHE_BYTES - 1, b'a');
        bytes.push(b'"');
        assert_eq!(bytes.len(), MAX_CACHE_BYTES);
        let parsed: Result<String, _> = deserialize_cache_content(&bytes);
        // At-cap content parses fine as a quoted string.
        parsed.expect("at-cap payload should parse");
    }

    #[test]
    fn deserialize_cache_content_propagates_json_error() {
        let bad = br#"{"items":not_json}"#;
        let err = deserialize_cache_content::<SmallPayload>(bad).expect_err("invalid JSON");
        assert!(matches!(err, CacheDeserializeError::Json(_)));
    }
}
