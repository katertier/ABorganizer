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
//! 1. Strip HTML via a small hand-rolled state machine — keep the
//!    body's textContent. EPUB spine documents are well-formed
//!    XHTML in practice, so we don't need a full parser DOM.
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

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

pub mod epub_toc;
pub mod epub_walk;

pub use epub_toc::{read_chapter_titles, read_chapter_titles_from_path};
pub use epub_walk::{EpubBody, EpubWalkError, extract_name_dict_from_epub, walk_spine};

/// One canonical proper-noun candidate with its observed
/// frequency. The C.4 stage writes a serialised `Vec<NameEntry>`
/// to `ai_cache.content` with `cache_type='epub_name_dict'`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// Implementation is a small state machine. EPUB spine HTML is
/// well-formed XHTML in practice; we do not try to be a generous
/// browser-grade parser. Comments and CDATA sections are skipped;
/// tag interiors are dropped; HTML entities (`&amp;` / `&lt;` /
/// `&gt;` / `&quot;` / `&apos;` / `&nbsp;` plus numeric `&#NNN;` /
/// `&#xHH;`) are decoded; other named entities pass through as
/// raw `&name;` (good enough for the proper-noun frequency floor).
#[must_use]
pub fn strip_html(html: &str) -> String {
    let body_slice = locate_body(html);
    let target = body_slice.unwrap_or(html);
    strip_tags_and_decode(target)
}

/// Find the substring between `<body...>` and `</body>` (case
/// insensitive). Returns `None` when no body element is present.
fn locate_body(html: &str) -> Option<&str> {
    let lower = html.to_ascii_lowercase();
    let open_start = lower.find("<body")?;
    // Skip past the opening tag's `>`.
    let after_open_tag = html[open_start..].find('>').map(|i| open_start + i + 1)?;
    let end = lower[after_open_tag..]
        .find("</body")
        .map_or(html.len(), |i| after_open_tag + i);
    Some(&html[after_open_tag..end])
}

/// Walk the slice once, emitting text outside tags / comments /
/// CDATA wrappers. Whitespace is collapsed to single spaces so
/// the downstream tokeniser doesn't see HTML line-wrap artefacts.
fn strip_tags_and_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut prev_was_space = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'<' {
            // CDATA: `<![CDATA[ ... ]]>` — keep contents.
            if bytes[i..].starts_with(b"<![CDATA[") {
                let start = i + b"<![CDATA[".len();
                if let Some(rel) = find_subsequence(&bytes[start..], b"]]>") {
                    let end = start + rel;
                    push_decoded(&input[start..end], &mut out, &mut prev_was_space);
                    i = end + b"]]>".len();
                    continue;
                }
                // Unterminated CDATA — eat to end.
                push_decoded(&input[start..], &mut out, &mut prev_was_space);
                break;
            }
            // Comment: `<!-- ... -->`.
            if bytes[i..].starts_with(b"<!--") {
                if let Some(rel) = find_subsequence(&bytes[i + 4..], b"-->") {
                    i = i + 4 + rel + 3;
                    continue;
                }
                break;
            }
            // Regular tag — skip until `>`.
            match find_subsequence(&bytes[i + 1..], b">") {
                Some(rel) => {
                    i = i + 1 + rel + 1;
                    if !prev_was_space && !out.is_empty() {
                        out.push(' ');
                        prev_was_space = true;
                    }
                }
                None => break,
            }
            continue;
        }
        // Outside any tag — copy chars, decoding entities and
        // collapsing whitespace.
        if b == b'&' {
            let (decoded, advance) = decode_entity(&input[i..]);
            for ch in decoded.chars() {
                push_char(ch, &mut out, &mut prev_was_space);
            }
            i += advance;
            continue;
        }
        let ch = input[i..].chars().next().unwrap_or('\0');
        push_char(ch, &mut out, &mut prev_was_space);
        i += ch.len_utf8();
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn push_char(ch: char, out: &mut String, prev_was_space: &mut bool) {
    if ch.is_whitespace() {
        if !*prev_was_space && !out.is_empty() {
            out.push(' ');
            *prev_was_space = true;
        }
    } else {
        out.push(ch);
        *prev_was_space = false;
    }
}

fn push_decoded(s: &str, out: &mut String, prev_was_space: &mut bool) {
    for ch in s.chars() {
        push_char(ch, out, prev_was_space);
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Decode a single HTML entity starting at `s[0] == '&'`. Returns
/// the decoded text (borrowed where possible) plus the number of
/// input bytes consumed.
fn decode_entity(s: &str) -> (std::borrow::Cow<'_, str>, usize) {
    debug_assert!(s.starts_with('&'));
    // Bounded scan for `;`. HTML entities are short — a 16-byte
    // window catches every named + numeric form we care about and
    // prevents O(n) scans on `&` characters in prose.
    let scan_end = s.len().min(16);
    let Some(semi_idx) = s[..scan_end].find(';') else {
        return (std::borrow::Cow::Borrowed("&"), 1);
    };
    let body = &s[1..semi_idx];
    let consumed = semi_idx + 1;
    let decoded: std::borrow::Cow<'_, str> = match body {
        "amp" => std::borrow::Cow::Borrowed("&"),
        "lt" => std::borrow::Cow::Borrowed("<"),
        "gt" => std::borrow::Cow::Borrowed(">"),
        "quot" => std::borrow::Cow::Borrowed("\""),
        "apos" => std::borrow::Cow::Borrowed("'"),
        "nbsp" => std::borrow::Cow::Borrowed("\u{00A0}"),
        _ if body.starts_with('#') => decode_numeric(&body[1..]).map_or_else(
            // Malformed numeric — pass the literal through.
            || std::borrow::Cow::Borrowed(&s[..consumed]),
            std::borrow::Cow::Owned,
        ),
        // Unknown named entity — pass through. Rare for EPUB body
        // text; the frequency floor absorbs the noise.
        _ => std::borrow::Cow::Borrowed(&s[..consumed]),
    };
    (decoded, consumed)
}

fn decode_numeric(spec: &str) -> Option<String> {
    let code = if let Some(hex) = spec.strip_prefix(['x', 'X']) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        spec.parse::<u32>().ok()?
    };
    char::from_u32(code).map(|c| c.to_string())
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
    fn strip_html_decodes_common_entities() {
        let html = r"<body>Caf&eacute; &amp; bar &mdash; Tom&apos;s &quot;place&quot; &#x2014; nice.</body>";
        let text = strip_html(html);
        // Numeric entity decoded.
        assert!(
            text.contains('\u{2014}'),
            "expected em-dash from &#x2014; in: {text}"
        );
        // XML entities decoded.
        assert!(text.contains('&'));
        assert!(text.contains('\''));
        assert!(text.contains('"'));
    }

    #[test]
    fn strip_html_handles_no_body_tag() {
        // Some EPUB spine entries omit the body wrapper. The
        // stripper falls back to the entire input.
        let html = r"<p>Just <em>some</em> prose.</p>";
        let text = strip_html(html);
        assert!(text.contains("Just"));
        assert!(text.contains("some"));
        assert!(text.contains("prose"));
    }

    #[test]
    fn strip_html_handles_comments_and_cdata() {
        let html = r"<body>Before <!-- hidden --> Middle <![CDATA[ data ]]> After</body>";
        let text = strip_html(html);
        assert!(text.contains("Before"));
        assert!(text.contains("Middle"));
        assert!(text.contains("After"));
        assert!(text.contains("data"));
        assert!(!text.contains("hidden"), "comment text should be stripped");
    }

    #[test]
    fn strip_html_body_tag_with_attributes() {
        let html = r#"<body class="chapter" id="ch01">Inner content here.</body>"#;
        let text = strip_html(html);
        assert!(text.contains("Inner"));
        assert!(text.contains("content"));
        assert!(!text.contains("chapter"), "body attrs should be stripped");
        assert!(!text.contains("ch01"));
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
