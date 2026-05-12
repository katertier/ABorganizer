//! Language detection via the same Swift FFI bridge as the
//! transcribe path. Wraps `NLLanguageRecognizer` from the
//! `NaturalLanguage` framework.
//!
//! Two call sites:
//!
//! 1. **Pre-transcribe** — feed concatenated tag-text
//!    (title + author + subtitle + tag values) to pick the
//!    locale we hand to `SpeechTranscriber`. Cheap; runs at
//!    job-creation time.
//! 2. **Post-transcribe** — feed transcript text past the
//!    publisher-jingle window (English on Audible regardless
//!    of book language). Produces a validation candidate;
//!    disagreement with the pre-transcribe pick is a signal
//!    to re-transcribe in the corrected locale.
//!
//! The Swift side returns null when the input is empty / pure
//! whitespace / `NLLanguageRecognizer` produces no hypothesis;
//! the Rust wrapper maps that to `Ok(None)` so callers can
//! distinguish "didn't detect" from "framework error."

use serde::{Deserialize, Serialize};

use ab_core::Result;

use crate::bridge::TranscriptSegment;

/// A single language hypothesis from `NLLanguageRecognizer`.
///
/// `code` is the framework's `NLLanguage` raw value — usually a
/// BCP-47 / ISO-639-1 string like `"en"`, `"de"`, or sometimes
/// a script-disambiguating variant like `"zh-Hans"` /
/// `"zh-Hant"`. Confidence is in `[0.0, 1.0]` and the
/// hypotheses sum to ~1.0 across a single recognizer run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LanguageHit {
    /// `NLLanguage` raw value. Caller maps to BCP-47 locale
    /// (e.g. `"en"` → `"en-US"`) using its own policy.
    pub language: String,
    /// Engine-reported probability in `[0.0, 1.0]`.
    pub confidence: f64,
}

/// Top hypothesis + up to N alternatives in descending
/// confidence order.
///
/// The dominant `language` is the engine's `dominantLanguage`
/// (highest-probability hit), not just `alternatives[0]` —
/// they're separate API calls under the hood.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LanguageDetection {
    /// Dominant language code (`NLLanguageRecognizer.dominantLanguage`).
    pub language: String,
    /// Confidence of the dominant hypothesis. May be 0 when
    /// `NLLanguageRecognizer` can't put a number on it (very
    /// short / ambiguous input).
    pub confidence: f64,
    /// Other hypotheses, dominant entry filtered out, in
    /// descending confidence. Length ≤ `max_alternatives`
    /// passed to `detect`.
    pub alternatives: Vec<LanguageHit>,
}

#[cfg(aborg_speech_bridge)]
#[expect(
    unsafe_code,
    reason = "FFI to Swift requires unsafe extern blocks and raw-pointer round-trips; safe wrappers exposed by the parent module are the public surface."
)]
mod ffi {
    use std::ffi::{c_char, c_void};

    use ab_core::{Error, Result};
    use tokio::sync::oneshot;

    use super::LanguageDetection;

    unsafe extern "C" {
        fn aborg_detect_language(
            text: *const c_char,
            max_alternatives: isize,
            ctx: *mut c_void,
            callback: unsafe extern "C" fn(*mut c_void, *const c_char, usize, i32),
        );
    }

    /// Callback fired exactly once by Swift. `code == 0` is the
    /// success path; `(ptr == null, code == 0)` means
    /// "inconclusive" → caller maps to `Ok(None)`. Any non-zero
    /// code is an FFI failure (currently only `kErrCodeEncodeFailure`
    /// for language).
    unsafe extern "C" fn on_result(ctx: *mut c_void, ptr: *const c_char, len: usize, code: i32) {
        if ctx.is_null() {
            return;
        }
        // SAFETY: paired with `Box::into_raw` in `detect_impl`;
        // Swift returns ctx unchanged exactly once.
        let sender = unsafe {
            Box::from_raw(ctx.cast::<oneshot::Sender<Result<Option<LanguageDetection>>>>())
        };
        let outcome: Result<Option<LanguageDetection>> = if code != 0 {
            Err(Error::stage(
                "language",
                format!("Swift bridge error code {code}"),
            ))
        } else if ptr.is_null() || len == 0 {
            Ok(None)
        } else {
            // SAFETY: Swift documents `(ptr, len)` as a UTF-8
            // JSON buffer of exactly `len` bytes. Lifetime is
            // the callback's duration; we copy via `to_owned`.
            let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
            std::str::from_utf8(slice)
                .map_err(|e| Error::stage("language", format!("non-utf8 buffer: {e}")))
                .and_then(|s| {
                    serde_json::from_str::<LanguageDetection>(s)
                        .map_err(|e| Error::stage("language", format!("json parse: {e}")))
                })
                .map(Some)
        };
        let _ = sender.send(outcome);
    }

    pub(super) async fn detect_impl(
        text: &str,
        max_alternatives: usize,
    ) -> Result<Option<LanguageDetection>> {
        let text_c = std::ffi::CString::new(text)
            .map_err(|e| Error::stage("language", format!("text has NUL byte: {e}")))?;
        // `usize` → `isize` for the Swift `Int` parameter. The
        // Swift side clamps to [0, 16] so any cast worry is
        // moot; an obviously-too-large value just gets capped.
        let max_n: isize = max_alternatives.try_into().unwrap_or(isize::MAX);

        let (tx, rx) = oneshot::channel::<Result<Option<LanguageDetection>>>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();

        // SAFETY: `text_c` outlives the synchronous portion of
        // the Swift call (its `CString` isn't dropped until this
        // fn returns). `aborg_detect_language` is synchronous
        // on the Swift side (no `await`) but it still fires the
        // callback exactly once before returning.
        unsafe {
            aborg_detect_language(text_c.as_ptr(), max_n, ctx, on_result);
        }
        rx.await
            .map_err(|_| Error::stage("language", "Swift dropped the callback without firing it"))?
    }
}

/// Detect language of `text`, returning the top hypothesis plus
/// up to `max_alternatives` runners-up.
///
/// Returns `Ok(None)` for empty / whitespace-only input or when
/// the framework declines to commit to any language (rare but
/// possible for very short / ambiguous input).
///
/// # Errors
///
/// - `Error::Stage("language", ...)` when the bridge isn't
///   linked (non-macOS / no-swiftc build).
/// - `Error::Stage("language", ...)` for FFI / parse failures.
pub async fn detect(text: &str, max_alternatives: usize) -> Result<Option<LanguageDetection>> {
    #[cfg(aborg_speech_bridge)]
    {
        ffi::detect_impl(text, max_alternatives).await
    }
    #[cfg(not(aborg_speech_bridge))]
    {
        let _ = (text, max_alternatives);
        Err(ab_core::Error::stage(
            "language",
            "NaturalLanguage FFI bridge not linked (non-macOS host or swiftc unavailable)",
        ))
    }
}

/// Detect language from transcript segments, dropping anything
/// whose end falls inside the first `skip_ms` of the file.
///
/// Why the skip: Audible's house jingle is always English
/// regardless of the book's language (~30 s); the publisher
/// jingle at the outer cut is similarly English-only. Including
/// those in the detection input biases short non-English samples.
///
/// Segments are joined with single spaces. Empty result after
/// the skip → `Ok(None)`.
///
/// # Errors
///
/// Same as [`detect`].
pub async fn detect_from_transcript(
    segments: &[TranscriptSegment],
    skip_ms: u64,
    max_alternatives: usize,
) -> Result<Option<LanguageDetection>> {
    // Skip segments that end *inside* the cut — segments that
    // straddle the boundary are still kept (their later half
    // contains book content). Same heuristic used by ABtagger's
    // audiologo-trim crosswalk.
    let mut text = String::new();
    for seg in segments {
        if seg.end_ms <= skip_ms {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&seg.text);
    }
    if text.trim().is_empty() {
        return Ok(None);
    }
    detect(&text, max_alternatives).await
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Either `Ok(None)` (bridge linked + framework returned no
    /// hypothesis) or `Err(_)` (bridge not linked at all) is a
    /// pass — both are valid "no detection" outcomes that the
    /// caller would treat the same way. Only `Ok(Some(_))` is a
    /// failure for these inputs.
    fn assert_no_detection(r: &Result<Option<LanguageDetection>>) {
        assert!(
            !matches!(r, Ok(Some(_))),
            "expected None or Err for inconclusive input, got {r:?}"
        );
    }

    #[tokio::test]
    async fn empty_input_returns_none_or_unavailable() {
        assert_no_detection(&detect("", 3).await);
    }

    #[tokio::test]
    async fn whitespace_input_returns_none_or_unavailable() {
        assert_no_detection(&detect("   \t\n  ", 3).await);
    }

    #[tokio::test]
    async fn nul_byte_in_text_rejected_cleanly() {
        // CString conversion catches this before the FFI call;
        // runs on every host.
        let r = detect("hello\0world", 3).await;
        assert!(r.is_err(), "NUL in text must Err, got {r:?}");
    }

    #[tokio::test]
    async fn skip_filters_segments_before_cutoff() {
        // Pure-Rust test of the segment-filter logic. Doesn't
        // touch the FFI: when only-pre-cutoff segments remain,
        // we return Ok(None) before calling Swift.
        let segs = vec![
            TranscriptSegment {
                start_ms: 0,
                end_ms: 2_000,
                text: "Hello".into(),
                confidence: 0.9,
            },
            TranscriptSegment {
                start_ms: 2_000,
                end_ms: 5_000,
                text: "World".into(),
                confidence: 0.9,
            },
        ];
        assert_no_detection(&detect_from_transcript(&segs, 10_000, 3).await);
    }
}
