//! ASIN auto-learn helper.
//!
//! Whenever the operator manually sets an ASIN on a book, we
//! record the (title, author, asin) mapping in
//! `asin_learnings`. The next audible-search run consults the
//! table before hitting the network: a hit short-circuits the
//! lookup with high confidence, a miss falls through to the
//! existing region walk.
//!
//! This module owns:
//!
//! * The normalisation function — case-fold + collapse internal
//!   whitespace. Stable across writers and readers; the hot-path
//!   lookup is a single index probe.
//! * The capture writer — INSERT OR IGNORE so a re-learn of the
//!   same (title, author, asin) triple is a no-op (UNIQUE index
//!   from migration 042).
//!
//! The consume side (audible-search hint) lives next door but
//! lands in a follow-up slice; that side will read from this
//! table and pick the highest `learned_at`.

use chrono::Utc;
use sqlx::{Sqlite, SqlitePool, Transaction};

/// Source tag for learnings captured from `PATCH
/// /api/v1/books/{id}`. Other future capture sites pick their own
/// tag (`'cli'`, `'batch-edit'`, `'voice'`).
pub const SOURCE_USER_EDIT: &str = "user_edit";

/// Provenance-source tag the audible-search stage writes when it
/// short-circuits the network call with a learned ASIN hint.
///
/// Lives in `book_field_provenance.source` and lets downstream
/// consumers (audit, UI, debug) tell a learned-hint hit from a
/// real Audible API result.
pub const PROVENANCE_SOURCE_LEARN: &str = "asin_learn";

/// Confidence written for ASINs sourced from a learning hit.
///
/// Sits between tag-supplied (0.7) and user-edit (1.0). The
/// operator validated this `(title, author) → asin` mapping on a
/// previous book; a new ingest with the same normalised key is a
/// strong signal but not as strong as a tag value the file itself
/// carries — different recordings / box-sets / region variants
/// can still share normalised metadata.
pub const ASIN_LEARN_CONFIDENCE: f64 = 0.8;

/// Normalise a free-form text field for indexed lookup.
///
/// Steps: lowercase via `to_lowercase`, collapse runs of internal
/// whitespace to single spaces, trim. Matches the convention used
/// by the consume-side lookup (same function — keep them in sync).
#[must_use]
pub fn normalise(raw: &str) -> String {
    let lower = raw.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_space = true;
    for ch in lower.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Look up the most recently learned ASIN for a normalised
/// `(title, author)` key. Returns `None` if either key normalises
/// empty or no learning row matches.
///
/// "Most recently learned" via `learned_at DESC` — if the operator
/// changed their mind about the right ASIN over time, the latest
/// edit wins.
///
/// # Errors
///
/// Returns the underlying [`sqlx::Error`] for SELECT failures.
pub async fn lookup(
    pool: &SqlitePool,
    title: &str,
    author: &str,
) -> Result<Option<String>, sqlx::Error> {
    let title_norm = normalise(title);
    let author_norm = normalise(author);
    if title_norm.is_empty() || author_norm.is_empty() {
        return Ok(None);
    }
    let row = sqlx::query!(
        r#"SELECT asin AS "asin!: String"
             FROM asin_learnings
            WHERE title_norm = ? AND author_norm = ?
            ORDER BY learned_at DESC
            LIMIT 1"#,
        title_norm,
        author_norm,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.asin))
}

/// Capture one `(title, author, asin)` learning inside the
/// caller's transaction. `title` and `author` are normalised
/// before insert.
///
/// Returns `Ok(true)` when a new row landed, `Ok(false)` when the
/// triple already existed (the UNIQUE constraint absorbed it).
///
/// `title` / `author` empty after normalisation → silent skip:
/// the row would never match anything on the consume side and
/// poisons the index with empty keys.
///
/// # Errors
///
/// Returns the underlying [`sqlx::Error`] for INSERT failures.
pub async fn capture(
    tx: &mut Transaction<'_, Sqlite>,
    title: &str,
    author: &str,
    asin: &str,
    source: &str,
) -> Result<bool, sqlx::Error> {
    let title_norm = normalise(title);
    let author_norm = normalise(author);
    if title_norm.is_empty() || author_norm.is_empty() || asin.is_empty() {
        return Ok(false);
    }
    let learned_at = Utc::now().to_rfc3339();
    let result = sqlx::query!(
        r#"INSERT OR IGNORE INTO asin_learnings
              (title_norm, author_norm, asin, source, learned_at)
            VALUES (?, ?, ?, ?, ?)"#,
        title_norm,
        author_norm,
        asin,
        source,
        learned_at,
    )
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn normalise_lowercases() {
        assert_eq!(normalise("Mistborn"), "mistborn");
    }

    #[test]
    fn normalise_collapses_internal_whitespace() {
        assert_eq!(normalise("The   Way  Of\tKings"), "the way of kings");
    }

    #[test]
    fn normalise_trims_edges() {
        assert_eq!(normalise("  Stormlight  "), "stormlight");
    }

    #[test]
    fn normalise_handles_unicode_lowercase() {
        // Greek capital → lowercase. Confirms we're not stuck on
        // ASCII-only `to_ascii_lowercase`.
        assert_eq!(normalise("ΟΔΥΣΣΕΙΑ"), "οδυσσεια");
    }
}
