//! Transcript-aided detection helpers (slice 4C; ADR-0024 §
//! "Phase 2: detection on non-Audible books" tiers 3 + 4).
//!
//! Two methods land here:
//!
//! - [`Method::TranscriptOnly`] — search `transcript_head` for
//!   publisher mentions (tier-4 vocab below); a hit becomes a
//!   candidate with the cut localized by the matched segment's
//!   `start_ms` / `end_ms` boundaries. Always candidate; never
//!   auto-applies.
//! - [`Method::FingerprintAndTranscript`] — when a transcript
//!   hit lines up with a `slide_match` fingerprint hit nearby
//!   (within `FP_TRANSCRIPT_PROXIMITY_MS`), promote the
//!   confidence to reflect the corroboration. Still candidate;
//!   the user reviews. (Auto-apply would risk false-positive
//!   double-counts.)
//!
//! Tier-4 vocab covers four sources:
//! 1. `publishers.name` from the library — case-insensitive
//!    substring match against the transcript.
//! 2. `authors.name` — same.
//! 3. `narrators.name` — same.
//! 4. The static const list in [`crate::phrases::PHRASES`].
//!
//! ## Scope window
//!
//! Tier-4 only scans the first
//! [`TRANSCRIPT_SCAN_HEAD_SECS`] seconds of the transcript head
//! (intro) and the last `TRANSCRIPT_SCAN_TAIL_SECS` of the
//! transcript tail (outro). Anything past those bounds is
//! deliberately out-of-tier — a publisher mention deep in the
//! body of the book is content, not a jingle.

use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use serde::Deserialize;

use crate::phrases::first_phrase_hit;
use crate::{Kind, Method};

/// Scan window for intro transcript publisher mentions (seconds).
///
/// Tighter than the audio fingerprint window because publisher
/// jingles vocalize within the first ~30 seconds; anything past
/// 60 s is virtually certain to be content.
pub const TRANSCRIPT_SCAN_HEAD_SECS: u64 = 60;

/// Scan window for outro transcript publisher mentions (seconds
/// counted back from end-of-transcript).
pub const TRANSCRIPT_SCAN_TAIL_SECS: u64 = 30;

/// Proximity (ms) within which a fingerprint hit "corroborates"
/// a transcript hit.
///
/// ADR-0024's `FingerprintAndTranscript` shape: fingerprint at
/// start, transcript localises the end. Default tolerance is
/// generous to handle chromaprint hash quantization.
pub const FP_TRANSCRIPT_PROXIMITY_MS: u64 = 2_000;

/// Confidence given to a tier-4 (`TranscriptOnly`) hit.
///
/// The transcript matched a phrase but we have no fingerprint;
/// confidence is below the auto-apply floor by design so the
/// user reviews every such candidate.
pub const TRANSCRIPT_ONLY_CONFIDENCE: f32 = 0.55;

/// Confidence given when a fingerprint hit AND a transcript hit
/// land near each other.
///
/// Two independent signals corroborate; confidence still under
/// the auto-apply floor (transcript-aided tiers never auto-apply
/// per ADR-0024).
pub const FP_AND_TRANSCRIPT_CONFIDENCE: f32 = 0.75;

/// One transcript segment cached in `ai_cache.content` under
/// `cache_type='transcript_head'` (or `_tail`). The shape comes
/// from the Speech bridge's `TranscriptSegment`.
#[derive(Debug, Clone, Deserialize)]
struct CachedSegment {
    start_ms: u64,
    end_ms: u64,
    text: String,
    #[allow(dead_code)]
    confidence: f32,
}

/// Wrapper matching the on-disk cache shape produced by the
/// transcribe stage.
#[derive(Debug, Clone, Deserialize)]
struct CachedTranscript {
    segments: Vec<CachedSegment>,
}

/// A transcript-aided detection candidate.
#[derive(Debug, Clone)]
pub struct TranscriptCandidate {
    /// Which Method produced this candidate.
    pub method: Method,
    /// Intro or outro?
    pub kind: Kind,
    /// Window-relative cut start (ms from file start, same as
    /// `book_file_audiologos.jingle_start_ms`).
    pub jingle_start_ms: u64,
    /// Window-relative cut end.
    pub jingle_end_ms: u64,
    /// Confidence in `[0.0, 1.0]`. Below auto-apply floor by
    /// design (transcript-aided tiers always candidate).
    pub confidence: f32,
    /// The matched publisher name (from PHRASES or
    /// `publishers.name`). Surfaced in the review UI.
    pub publisher_hint: Option<String>,
}

/// Search the cached intro transcript for a tier-4 publisher
/// mention. Returns a candidate when a phrase / publisher /
/// author / narrator name matches inside the head scan window.
///
/// Independent of fingerprint matching. The caller decides
/// whether to combine this with a fingerprint hit (producing
/// `Method::FingerprintAndTranscript`) or stand-alone
/// (`Method::TranscriptOnly`).
///
/// # Errors
///
/// Returns [`Error::Database`] on DB failure. Bad-shape cache
/// rows (malformed JSON) log + return Ok(None).
pub async fn detect_intro_via_transcript(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<TranscriptCandidate>> {
    let Some(segments) = load_head_segments(library, book_id).await? else {
        return Ok(None);
    };
    let scan_end_ms = TRANSCRIPT_SCAN_HEAD_SECS.saturating_mul(1000);
    scan_for_phrase(&segments, 0, scan_end_ms, Kind::Intro, library, book_id).await
}

/// Search the cached outro transcript for a tier-4 publisher
/// mention. Symmetric to [`detect_intro_via_transcript`].
///
/// # Errors
///
/// Returns [`Error::Database`] on DB failure.
pub async fn detect_outro_via_transcript(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<TranscriptCandidate>> {
    let Some(segments) = load_tail_segments(library, book_id).await? else {
        return Ok(None);
    };
    // The tail transcript's time-base is file-relative (per the
    // speech bridge contract). The last segment's `end_ms` is
    // effectively the audio end; scan back from there.
    let Some(last) = segments.last() else {
        return Ok(None);
    };
    let tail_end = last.end_ms;
    let scan_start = tail_end.saturating_sub(TRANSCRIPT_SCAN_TAIL_SECS.saturating_mul(1000));
    scan_for_phrase(
        &segments,
        scan_start,
        tail_end,
        Kind::Outro,
        library,
        book_id,
    )
    .await
}

/// Promote a tier-4 candidate when a fingerprint hit lands nearby.
///
/// Per ADR-0024: when both signals fire, confidence rises to the
/// `FingerprintAndTranscript` tier and the method changes. Still
/// never auto-applies (`Method::FingerprintAndTranscript` has a
/// 0.0 floor by default).
#[must_use]
pub fn corroborate_with_fingerprint(
    transcript_cand: &TranscriptCandidate,
    fingerprint_hit_start_ms: u64,
) -> Option<TranscriptCandidate> {
    let dist = transcript_cand
        .jingle_start_ms
        .abs_diff(fingerprint_hit_start_ms);
    if dist > FP_TRANSCRIPT_PROXIMITY_MS {
        return None;
    }
    Some(TranscriptCandidate {
        method: Method::FingerprintAndTranscript,
        confidence: FP_AND_TRANSCRIPT_CONFIDENCE,
        // Other fields unchanged; both signals agreed on the
        // location ±2 s, so the transcript's start_ms wins for
        // tightness.
        ..transcript_cand.clone()
    })
}

// ── Helpers ──────────────────────────────────────────────────

/// Concatenated-transcript segment boundaries.
///
/// `localize_hit` maps a byte offset inside the joined string
/// back to the original segment's `start_ms` / `end_ms` via this.
#[derive(Debug, Clone, Copy)]
struct LiveRange {
    start_ms: u64,
    end_ms: u64,
    text_offset: usize,
}

/// Scan `segments` in `[scan_start_ms, scan_end_ms]` for any
/// phrase / publisher / author / narrator hit. Returns the
/// earliest match as a [`TranscriptCandidate`].
#[allow(
    clippy::too_many_arguments,
    reason = "Each arg is a structural input to the scan; bundling them adds indirection without simplifying the call site."
)]
async fn scan_for_phrase(
    segments: &[CachedSegment],
    scan_start_ms: u64,
    scan_end_ms: u64,
    kind: Kind,
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<TranscriptCandidate>> {
    let mut joined = String::new();
    let mut ranges: Vec<LiveRange> = Vec::new();
    for s in segments {
        if s.end_ms <= scan_start_ms {
            continue;
        }
        if s.start_ms >= scan_end_ms {
            break;
        }
        if !joined.is_empty() {
            joined.push(' ');
        }
        ranges.push(LiveRange {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            text_offset: joined.len(),
        });
        joined.push_str(&s.text);
    }
    if joined.is_empty() {
        return Ok(None);
    }
    let lower = joined.to_lowercase();

    // Try the const phrase list first — it carries a publisher
    // hint, which we surface to the review UI.
    if let Some(hit) = first_phrase_hit(&lower) {
        return Ok(Some(localize_hit(
            &ranges,
            hit.byte_offset,
            hit.phrase.text.len(),
            kind,
            Some(hit.phrase.publisher.to_owned()),
        )));
    }

    // Then walk publishers / authors / narrators from the
    // library. Done in that order so `publishers.name` matches
    // win the publisher attribution; author / narrator hits
    // record only the position.
    if let Some(hit) = match_name_table(&lower, library, NameTable::Publishers).await? {
        let len = hit.1.len();
        return Ok(Some(localize_hit(&ranges, hit.0, len, kind, Some(hit.1))));
    }
    if let Some(hit) = match_name_table(&lower, library, NameTable::Authors).await? {
        return Ok(Some(localize_hit(&ranges, hit.0, hit.1.len(), kind, None)));
    }
    if let Some(hit) = match_name_table(&lower, library, NameTable::Narrators).await? {
        return Ok(Some(localize_hit(&ranges, hit.0, hit.1.len(), kind, None)));
    }

    let _ = book_id; // Surface in tracing once wired into stage.
    Ok(None)
}

/// Map a byte-offset hit inside the joined transcript text back
/// to the segment range it covers, producing a candidate.
fn localize_hit(
    ranges: &[LiveRange],
    byte_offset: usize,
    match_len: usize,
    kind: Kind,
    publisher_hint: Option<String>,
) -> TranscriptCandidate {
    let match_end_byte = byte_offset + match_len;
    let mut start_ms = ranges.first().map_or(0, |r| r.start_ms);
    let mut end_ms = start_ms;
    for r in ranges {
        if r.text_offset <= byte_offset {
            start_ms = r.start_ms;
        }
        if r.text_offset <= match_end_byte {
            end_ms = r.end_ms;
        }
    }
    if end_ms <= start_ms {
        // Degenerate (single-segment, zero-length): give it a
        // minimum 1 s window so downstream maths doesn't blow up.
        end_ms = start_ms.saturating_add(1_000);
    }
    TranscriptCandidate {
        method: Method::TranscriptOnly,
        kind,
        jingle_start_ms: start_ms,
        jingle_end_ms: end_ms,
        confidence: TRANSCRIPT_ONLY_CONFIDENCE,
        publisher_hint,
    }
}

#[derive(Debug, Clone, Copy)]
enum NameTable {
    Publishers,
    Authors,
    Narrators,
}

impl NameTable {
    const fn query(self) -> &'static str {
        match self {
            Self::Publishers => "SELECT name FROM publishers",
            Self::Authors => "SELECT name FROM authors",
            Self::Narrators => "SELECT name FROM narrators",
        }
    }
}

/// Walk the `query`'s `name` results; return the first
/// `(byte_offset, original_name)` hit in `text_lowercased`.
///
/// The name is lowercased for the match; the returned tuple's
/// name preserves the DB's original casing for the
/// `publisher_hint`.
async fn match_name_table(
    text_lowercased: &str,
    library: &LibraryDb,
    table: NameTable,
) -> Result<Option<(usize, String)>> {
    let rows: Vec<(String,)> = sqlx::query_as(table.query())
        .fetch_all(library.pool())
        .await
        .map_err(|e| Error::Database(format!("transcript-aided name walk: {e}")))?;
    let mut best: Option<(usize, String)> = None;
    for (name,) in rows {
        if name.is_empty() {
            continue;
        }
        let name_lower = name.to_lowercase();
        if let Some(pos) = text_lowercased.find(&name_lower) {
            if best.as_ref().is_none_or(|b| pos < b.0) {
                best = Some((pos, name));
            }
        }
    }
    Ok(best)
}

async fn load_head_segments(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<Vec<CachedSegment>>> {
    load_cached_segments(library, book_id, "transcript_head").await
}

async fn load_tail_segments(
    library: &LibraryDb,
    book_id: BookId,
) -> Result<Option<Vec<CachedSegment>>> {
    load_cached_segments(library, book_id, "transcript_tail").await
}

async fn load_cached_segments(
    library: &LibraryDb,
    book_id: BookId,
    cache_type: &str,
) -> Result<Option<Vec<CachedSegment>>> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT content FROM ai_cache WHERE book_id = ? AND cache_type = ?",
        id,
        cache_type,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("transcript-aided cache lookup: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(bytes) = row.content else {
        return Ok(None);
    };
    let parsed: CachedTranscript = match ab_core::cache::deserialize_cache_content(&bytes) {
        Ok(p) => p,
        Err(e) => {
            // B.2a: covers JSON parse failures + oversized payloads.
            tracing::warn!(
                book = %book_id,
                cache_type,
                error = %e,
                "audiologo.transcript_aided.cache_parse_failed"
            );
            return Ok(None);
        }
    };
    if parsed.segments.is_empty() {
        return Ok(None);
    }
    Ok(Some(parsed.segments))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    async fn fresh_library(dir: &std::path::Path) -> LibraryDb {
        LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
            .await
            .expect("open library")
    }

    fn seg(start_ms: u64, end_ms: u64, text: &str) -> CachedSegment {
        CachedSegment {
            start_ms,
            end_ms,
            text: text.into(),
            confidence: 1.0,
        }
    }

    async fn seed_transcript(
        library: &LibraryDb,
        book_id: i64,
        cache_type: &str,
        segments: &[CachedSegment],
    ) {
        let payload = serde_json::json!({
            "segments": segments.iter().map(|s| serde_json::json!({
                "start_ms": s.start_ms,
                "end_ms": s.end_ms,
                "text": s.text,
                "confidence": s.confidence,
            })).collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_vec(&payload).expect("serialize");
        sqlx::query(
            "INSERT INTO ai_cache (book_id, cache_type, content, compressed, extractor_version) \
             VALUES (?, ?, ?, 0, 'test')",
        )
        .bind(book_id)
        .bind(cache_type)
        .bind(&bytes)
        .execute(library.pool())
        .await
        .expect("seed transcript");
    }

    #[tokio::test]
    async fn intro_detects_audible_studios_in_first_segment() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(library.pool())
            .await
            .expect("seed book");
        seed_transcript(
            &library,
            1,
            "transcript_head",
            &[
                seg(0, 4_000, "Audible Studios presents"),
                seg(4_000, 8_000, "Foundation by Isaac Asimov"),
            ],
        )
        .await;

        let hit = detect_intro_via_transcript(&library, BookId(1))
            .await
            .expect("scan")
            .expect("hit");
        assert_eq!(hit.method, Method::TranscriptOnly);
        assert_eq!(hit.kind, Kind::Intro);
        assert_eq!(hit.jingle_start_ms, 0);
        assert_eq!(hit.jingle_end_ms, 4_000);
        assert_eq!(hit.publisher_hint.as_deref(), Some("Audible Studios"));
        assert!((hit.confidence - TRANSCRIPT_ONLY_CONFIDENCE).abs() < 1e-3);
    }

    #[tokio::test]
    async fn intro_returns_none_when_no_phrase_or_name_matches() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(library.pool())
            .await
            .expect("seed book");
        seed_transcript(
            &library,
            1,
            "transcript_head",
            &[seg(0, 4_000, "Chapter one. The book begins.")],
        )
        .await;

        let hit = detect_intro_via_transcript(&library, BookId(1))
            .await
            .expect("scan");
        assert!(hit.is_none());
    }

    #[tokio::test]
    async fn intro_picks_up_publisher_table_name() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(library.pool())
            .await
            .expect("seed book");
        // Custom publisher not in PHRASES.
        sqlx::query("INSERT INTO publishers (publisher_id, name) VALUES (1, 'Boutique Audio')")
            .execute(library.pool())
            .await
            .expect("seed publisher");
        seed_transcript(
            &library,
            1,
            "transcript_head",
            &[seg(0, 5_000, "Welcome to Boutique Audio.")],
        )
        .await;

        let hit = detect_intro_via_transcript(&library, BookId(1))
            .await
            .expect("scan")
            .expect("hit");
        assert_eq!(hit.publisher_hint.as_deref(), Some("Boutique Audio"));
    }

    #[tokio::test]
    async fn intro_skips_phrases_beyond_scan_window() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(library.pool())
            .await
            .expect("seed book");
        // Phrase at minute 5 (well past the 60s scan window).
        seed_transcript(
            &library,
            1,
            "transcript_head",
            &[
                seg(0, 30_000, "Chapter one."),
                seg(300_000, 305_000, "Audible Studios presents"),
            ],
        )
        .await;

        let hit = detect_intro_via_transcript(&library, BookId(1))
            .await
            .expect("scan");
        assert!(
            hit.is_none(),
            "scan window is the first 60s; phrase at 5min must miss"
        );
    }

    #[tokio::test]
    async fn corroborate_promotes_when_fingerprint_nearby() {
        let cand = TranscriptCandidate {
            method: Method::TranscriptOnly,
            kind: Kind::Intro,
            jingle_start_ms: 1_500,
            jingle_end_ms: 5_000,
            confidence: TRANSCRIPT_ONLY_CONFIDENCE,
            publisher_hint: Some("Audible Studios".into()),
        };
        // Fingerprint hit at 1500 ms; well within proximity.
        let corroborated = corroborate_with_fingerprint(&cand, 1_500).expect("corroborated");
        assert_eq!(corroborated.method, Method::FingerprintAndTranscript);
        assert!((corroborated.confidence - FP_AND_TRANSCRIPT_CONFIDENCE).abs() < 1e-3);
    }

    #[tokio::test]
    async fn corroborate_returns_none_when_fingerprint_too_far() {
        let cand = TranscriptCandidate {
            method: Method::TranscriptOnly,
            kind: Kind::Intro,
            jingle_start_ms: 1_500,
            jingle_end_ms: 5_000,
            confidence: TRANSCRIPT_ONLY_CONFIDENCE,
            publisher_hint: None,
        };
        // 10s away — well past the 2s proximity threshold.
        assert!(corroborate_with_fingerprint(&cand, 11_500).is_none());
    }

    #[tokio::test]
    async fn no_cache_returns_none_cleanly() {
        let tmp = TempDir::new().expect("tmpdir");
        let library = fresh_library(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'fixture')")
            .execute(library.pool())
            .await
            .expect("seed book");
        // No transcript_head cache row.
        let hit = detect_intro_via_transcript(&library, BookId(1))
            .await
            .expect("scan");
        assert!(hit.is_none());
    }
}
