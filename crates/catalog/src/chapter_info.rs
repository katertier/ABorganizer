//! Audible chapter-info JSON parser (ADR-0050 § 3, partial).
//!
//! Audible exposes `/1.0/content/{asin}/metadata?response_groups=chapter_info`
//! returning a content-metadata document that carries the canonical
//! per-chapter timestamps Audible itself uses. Two fields are
//! interesting beyond the chapter list:
//!
//! * `brand_intro_duration_ms` — duration of the standard
//!   front-loaded Audible jingle ("Audible, an Amazon company…").
//! * `brand_outro_duration_ms` — duration of the back-loaded jingle.
//! * `is_accurate` — Audible's own confidence flag on the chapter
//!   timings. `true` means the timings are master-rendered;
//!   `false` means they were inferred from somewhere noisier and
//!   may drift from actual audio boundaries by hundreds of ms.
//!
//! When `is_accurate == true`, the brand-duration values are
//! sample-accurate boundaries we can use as a strong prior in
//! audiologo detection (ADR-0024 Tier 1+) — the silence-confirm
//! helper just verifies they land in silence and we cut.
//!
//! When `is_accurate == false`, we record the values but the
//! acoustic detector retains authority — no cutting based on
//! Audible's numbers alone.
//!
//! This slice ships the parser only. Follow-up slices wire the
//! HTTP fetch path, the schema migration (3 new columns on the
//! audiologo candidates row), and the audiologo detector
//! integration.

use serde::Deserialize;

/// Subset of the `content_metadata.chapter_info` JSON we need.
///
/// Audible's response is much larger — we deliberately deserialize
/// only the fields ADR-0050 § 3 + ADR-0024 use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChapterInfo {
    /// Audible's confidence flag on the chapter timings. Maps from
    /// `is_accurate`.
    #[serde(default)]
    pub is_accurate: bool,
    /// Standard front-loaded Audible jingle duration. Maps from
    /// `brand_intro_duration_ms`. Always non-negative; `0` means
    /// "no detected intro" rather than "unmeasured."
    #[serde(default, rename = "brand_intro_duration_ms")]
    pub brand_intro_ms: u32,
    /// Standard back-loaded Audible jingle duration. Maps from
    /// `brand_outro_duration_ms`.
    #[serde(default, rename = "brand_outro_duration_ms")]
    pub brand_outro_ms: u32,
    /// Total audio runtime per Audible's own measurement, in ms.
    /// Useful as a sanity check against `lofty`-reported duration.
    /// Maps from `runtime_length_ms`. `None` if absent.
    #[serde(rename = "runtime_length_ms")]
    pub runtime_ms: Option<u64>,
}

/// Wrapper for the outer `content_metadata` envelope Audible
/// returns. The interesting bits live under
/// `content_metadata.chapter_info`.
#[derive(Debug, Clone, Deserialize)]
struct ContentMetadata {
    chapter_info: Option<ChapterInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct Envelope {
    content_metadata: Option<ContentMetadata>,
}

/// Parse the full Audible `/1.0/content/{asin}/metadata` response
/// body, returning the chapter-info subset.
///
/// `None` when the response is missing the nested
/// `content_metadata.chapter_info` block — Audible occasionally
/// returns the envelope with chapter info absent for books that
/// haven't shipped chapter timings yet. Callers route this to
/// "fall back to acoustic detection only."
///
/// # Errors
///
/// [`serde_json::Error`] on malformed JSON. Missing fields inside
/// `chapter_info` are not errors — they default per `serde(default)`.
pub fn parse_response(body: &str) -> Result<Option<ChapterInfo>, serde_json::Error> {
    let env: Envelope = serde_json::from_str(body)?;
    Ok(env.content_metadata.and_then(|m| m.chapter_info))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_response() {
        let body = r#"{
            "content_metadata": {
                "chapter_info": {
                    "is_accurate": true,
                    "brand_intro_duration_ms": 2840,
                    "brand_outro_duration_ms": 5860,
                    "runtime_length_ms": 41940000
                }
            }
        }"#;
        let ci = parse_response(body).expect("parse").expect("present");
        assert!(ci.is_accurate);
        assert_eq!(ci.brand_intro_ms, 2840);
        assert_eq!(ci.brand_outro_ms, 5860);
        assert_eq!(ci.runtime_ms, Some(41_940_000));
    }

    #[test]
    fn missing_chapter_info_returns_none() {
        let body = r#"{
            "content_metadata": {
                "other_field": "value"
            }
        }"#;
        assert!(parse_response(body).expect("parse").is_none());
    }

    #[test]
    fn missing_content_metadata_returns_none() {
        let body = "{}";
        assert!(parse_response(body).expect("parse").is_none());
    }

    #[test]
    fn is_accurate_defaults_false() {
        // Audible occasionally omits is_accurate; defaults to false
        // (the conservative choice — don't trust unflagged timings).
        let body = r#"{
            "content_metadata": {
                "chapter_info": {
                    "brand_intro_duration_ms": 1000
                }
            }
        }"#;
        let ci = parse_response(body).expect("parse").expect("present");
        assert!(!ci.is_accurate);
        assert_eq!(ci.brand_intro_ms, 1000);
        assert_eq!(ci.brand_outro_ms, 0);
        assert_eq!(ci.runtime_ms, None);
    }

    #[test]
    fn brand_durations_default_zero() {
        let body = r#"{
            "content_metadata": {
                "chapter_info": {
                    "is_accurate": true
                }
            }
        }"#;
        let ci = parse_response(body).expect("parse").expect("present");
        assert!(ci.is_accurate);
        assert_eq!(ci.brand_intro_ms, 0);
        assert_eq!(ci.brand_outro_ms, 0);
    }

    #[test]
    fn malformed_json_returns_err() {
        let body = "not json";
        assert!(parse_response(body).is_err());
    }

    #[test]
    fn extra_fields_inside_chapter_info_are_ignored() {
        // Audible's chapter_info has many fields we don't decode
        // (the per-chapter list, brandIntroDurationMs camelCase
        // variant, etc.). Trailing fields must not break parsing.
        let body = r#"{
            "content_metadata": {
                "chapter_info": {
                    "is_accurate": true,
                    "brand_intro_duration_ms": 2840,
                    "brand_outro_duration_ms": 5860,
                    "runtime_length_ms": 41940000,
                    "chapters": [{"title": "Chapter 1", "start_offset_ms": 0}],
                    "brandIntroDurationMs": 2840,
                    "is_chaptered": true
                }
            }
        }"#;
        let ci = parse_response(body).expect("parse").expect("present");
        assert!(ci.is_accurate);
        assert_eq!(ci.brand_intro_ms, 2840);
    }

    #[test]
    fn runtime_zero_is_some_zero_not_none() {
        // Distinguish "field missing" (None) from "Audible reports
        // 0 ms" (Some(0)). Either is suspicious but the caller's
        // policy may differ.
        let body = r#"{
            "content_metadata": {
                "chapter_info": {
                    "runtime_length_ms": 0
                }
            }
        }"#;
        let ci = parse_response(body).expect("parse").expect("present");
        assert_eq!(ci.runtime_ms, Some(0));
    }

    #[test]
    fn deeply_nested_unrelated_fields_dont_break_parse() {
        // Defensive: Audible's response also carries
        // `customer_rights`, `last_position_heard`, etc. Ensure
        // additional siblings at the content_metadata level don't
        // break the parse.
        let body = r#"{
            "content_metadata": {
                "chapter_info": {
                    "is_accurate": true,
                    "brand_intro_duration_ms": 100
                },
                "customer_rights": {"is_consumable": true},
                "last_position_heard": {"position_ms": 12345}
            }
        }"#;
        let ci = parse_response(body).expect("parse").expect("present");
        assert!(ci.is_accurate);
        assert_eq!(ci.brand_intro_ms, 100);
    }
}
