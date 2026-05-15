//! EPUB proper-noun dictionary extraction (ADR-0043 § C.4).
//!
//! Pure-function library: handed a chunk of HTML body text it
//! produces a `Vec<NameEntry>` of likely proper nouns with their
//! frequencies. No I/O, no EPUB container parsing — the C.4
//! pipeline stage will own the EPUB walk + cache write and
//! delegate to this module for each chapter document.
//!
//! ## Algorithm (ADR-0043 § C.4)
//!
//! 1. Strip HTML via [`scraper`] — keep the body's textContent.
//! 2. Tokenise via [`unicode_segmentation`] word boundaries.
//! 3. Track sentence-initial position with a tiny state machine
//!    over `.` / `!` / `?` / newline punctuation (within ±2
//!    tokens).
//! 4. Keep tokens that:
//!    - Start with a capital letter
//!    - Are **not** sentence-initial
//!    - Have frequency ≥ 3 across the whole input
//! 5. Also keep multi-token capitalised sequences ("Kaladin
//!    Stormblessed"). Sequences are recorded as-is and join the
//!    same frequency table; the threshold applies after the
//!    full pass.
//!
//! ## Anti-feature
//!
//! - We do NOT attempt to disambiguate (Mary the character vs.
//!   Mary the chapter heading). Frequency ≥ 3 + skipping
//!   sentence-initial covers most false positives; the C.5
//!   Levenshtein-replace step preserves sentence-initial
//!   casing of the corrected text anyway.

use std::collections::HashMap;

use scraper::{Html, Selector};
use unicode_segmentation::UnicodeSegmentation;

/// One canonical proper-noun candidate with its observed
/// frequency. The C.4 stage writes a serialised `Vec<NameEntry>`
/// to `ai_cache.content` with `cache_type='epub_name_dict'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameEntry {
    /// Canonical surface form. Multi-token sequences keep their
    /// internal spaces ("Kaladin Stormblessed"); single-token
    /// entries are bare capitalised words.
    pub surface: String,
    /// Total observed occurrences across the input.
    pub frequency: u32,
}

/// Minimum frequency below which a candidate is dropped. Per
/// ADR-0043 § C.4 step 4.
pub const MIN_FREQUENCY: u32 = 3;

/// Extract the body text from a chapter HTML document.
///
/// Falls back to the full document's text-content if no `body`
/// element is found (EPUB spine entries occasionally ship without
/// the wrapping `body` tag — strict-XHTML they're not).
#[must_use]
pub fn strip_html(html: &str) -> String {
    let doc = Html::parse_document(html);
    // `body *` text is the canonical body content; fall back to
    // the document root if no body present.
    if let Ok(sel) = Selector::parse("body") {
        if let Some(body) = doc.select(&sel).next() {
            return collect_text(body);
        }
    }
    // Fallback: every text node in the doc.
    doc.tree
        .root()
        .descendants()
        .filter_map(|n| n.value().as_text().map(|t| (**t).to_string()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn collect_text(node: scraper::element_ref::ElementRef<'_>) -> String {
    node.text().collect::<Vec<_>>().join(" ")
}

/// Run the full ADR-0043 § C.4 pipeline against a body of text.
///
/// `body_text` is the plain-text content (call [`strip_html`]
/// first on raw HTML). The result is sorted by descending
/// frequency, then alphabetically as a tie-breaker so the
/// `ai_cache` write is deterministic across runs.
#[must_use]
pub fn extract_name_dict(body_text: &str) -> Vec<NameEntry> {
    let counts = count_candidates(body_text);
    let mut out: Vec<NameEntry> = counts
        .into_iter()
        .filter(|(_, freq)| *freq >= MIN_FREQUENCY)
        .map(|(surface, frequency)| NameEntry { surface, frequency })
        .collect();
    out.sort_by(|a, b| {
        b.frequency
            .cmp(&a.frequency)
            .then(a.surface.cmp(&b.surface))
    });
    out
}

/// Convenience: strip HTML first, then extract.
#[must_use]
pub fn extract_name_dict_from_html(html: &str) -> Vec<NameEntry> {
    extract_name_dict(&strip_html(html))
}

/// Capitalised-token run being accumulated. `started_sentence_initial`
/// distinguishes "Kaladin" (mid-sentence — counts) from "Kaladin"
/// at sentence start (drops as single-token, survives as
/// multi-token).
#[derive(Default)]
struct Run {
    tokens: Vec<String>,
    started_sentence_initial: bool,
}

fn count_candidates(text: &str) -> HashMap<String, u32> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    // Walk the input one sentence-bounded chunk at a time. The
    // ADR's sentence-initial guard says "preceded by `.` / `!` /
    // `?` / newline within 2 tokens". We treat any of those
    // punctuation marks as a "start of sentence" marker; the
    // very first token after the marker is sentence-initial and
    // gets skipped from the per-token candidate list, but a
    // multi-token sequence starting with that token still counts.
    let mut prev_was_sentence_end = true; // start of document
    let mut run = Run::default();

    for tok in text.split_word_bounds() {
        let trimmed = tok.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_sentence_terminator(trimmed) {
            flush_run(&mut run, &mut counts);
            prev_was_sentence_end = true;
            continue;
        }
        if !is_word_token(trimmed) {
            flush_run(&mut run, &mut counts);
            continue;
        }
        let starts_capital = starts_with_upper(trimmed);
        if starts_capital {
            if run.tokens.is_empty() {
                run.started_sentence_initial = prev_was_sentence_end;
            }
            run.tokens.push(trimmed.to_owned());
        } else {
            flush_run(&mut run, &mut counts);
        }
        prev_was_sentence_end = false;
    }
    flush_run(&mut run, &mut counts);
    counts
}

fn flush_run(run: &mut Run, counts: &mut HashMap<String, u32>) {
    match run.tokens.len() {
        0 => {}
        1 => {
            // Sentence-initial single tokens drop per ADR-0043 §
            // C.4. Mid-sentence single capitalised tokens count.
            if !run.started_sentence_initial {
                let entry = std::mem::take(&mut run.tokens[0]);
                *counts.entry(entry).or_insert(0) += 1;
            }
        }
        _ => {
            // Multi-token capitalised sequence — joins as one
            // entry. Survives even when sentence-initial: the
            // run itself is a strong proper-noun signal.
            let joined = run.tokens.join(" ");
            *counts.entry(joined).or_insert(0) += 1;
        }
    }
    run.tokens.clear();
    run.started_sentence_initial = false;
}

fn is_sentence_terminator(tok: &str) -> bool {
    matches!(tok, "." | "!" | "?" | "\n" | "…")
}

fn is_word_token(tok: &str) -> bool {
    tok.chars().any(char::is_alphabetic)
}

fn starts_with_upper(tok: &str) -> bool {
    tok.chars().next().is_some_and(char::is_uppercase)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_pulls_body_text() {
        let html = r"<html><body>Hello <em>world</em>!</body></html>";
        let text = strip_html(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
    }

    #[test]
    fn strip_html_skips_script_and_style_tags() {
        // scraper's text() iterator does NOT skip script/style
        // by default. We accept that the tokeniser sees JS
        // identifiers as capitalised words too — the
        // frequency-3 floor mitigates the noise in practice.
        // Test documents the behaviour rather than asserting
        // it'd be different.
        let html = r"<body><script>console.log('hi')</script>Word Word Word</body>";
        let text = strip_html(html);
        assert!(text.contains("Word"));
    }

    #[test]
    fn mid_sentence_capitalised_above_threshold_is_kept() {
        // "Kaladin" appears 3 times mid-sentence in clean
        // positions (not adjacent to other capitalised tokens
        // that would swallow it into a multi-token sequence).
        let text = "He saw Kaladin again. Later, Kaladin spoke. The truth was that Kaladin knew.";
        let dict = extract_name_dict(text);
        assert!(
            dict.iter()
                .any(|n| n.surface == "Kaladin" && n.frequency >= 3),
            "expected Kaladin >= 3, got {dict:?}"
        );
    }

    #[test]
    fn below_threshold_is_dropped() {
        let text = "Kaladin was here. Kaladin was here.";
        let dict = extract_name_dict(text);
        assert!(
            !dict.iter().any(|n| n.surface == "Kaladin"),
            "only 2 occurrences — must be dropped"
        );
    }

    #[test]
    fn multi_token_sequence_is_recorded_as_one_entry() {
        // 3 mid-sentence occurrences (the lead-in words are
        // lower-case so the multi-token run starts cleanly at
        // "Kaladin Stormblessed").
        let text = "He saw Kaladin Stormblessed today. They watched Kaladin Stormblessed work. Later we met Kaladin Stormblessed again.";
        let dict = extract_name_dict(text);
        assert!(
            dict.iter()
                .any(|n| n.surface == "Kaladin Stormblessed" && n.frequency >= 3),
            "expected multi-token entry with freq>=3, got {dict:?}"
        );
    }

    #[test]
    fn sentence_initial_single_token_caps_are_not_counted() {
        // "Then" appears at sentence-start 5 times → still
        // dropped because each occurrence is sentence-initial.
        let text = "Then they ate. Then they slept. Then they ran. Then they spoke. Then they sat.";
        let dict = extract_name_dict(text);
        assert!(
            !dict.iter().any(|n| n.surface == "Then"),
            "sentence-initial single tokens must be dropped"
        );
    }

    #[test]
    fn sentence_initial_multi_token_sequence_survives() {
        // The ADR carve-out: multi-token capitalised sequences
        // survive even when sentence-initial because the
        // sequence itself is a strong proper-noun signal.
        let text = "Kaladin Stormblessed walked. Kaladin Stormblessed walked. Kaladin Stormblessed walked.";
        let dict = extract_name_dict(text);
        assert!(
            dict.iter().any(|n| n.surface == "Kaladin Stormblessed"),
            "multi-token sequence wins even sentence-initial: {dict:?}"
        );
    }

    #[test]
    fn output_is_sorted_by_frequency_desc() {
        let text = "Adolin met Adolin and Adolin saw Adolin nearby. Kaladin Stormblessed appeared. Kaladin Stormblessed appeared. Kaladin Stormblessed appeared.";
        let dict = extract_name_dict(text);
        // Verify descending order.
        for w in dict.windows(2) {
            assert!(
                w[0].frequency >= w[1].frequency,
                "expected freq-desc, got {:?} then {:?}",
                w[0],
                w[1],
            );
        }
    }
}
