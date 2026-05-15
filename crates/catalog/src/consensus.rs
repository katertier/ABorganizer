//! Consensus stage — promote provenance winners into `books`.
//!
//! Up until this stage, enrichment writes candidates into
//! `book_field_provenance` but never updates the user-visible
//! `books.<col>` columns. Readers (player UI, library list, ABS
//! API) join against `books` directly, so without consensus
//! they'd see scan's filename-derived placeholder titles forever.
//!
//! # Promotion rule
//!
//! For each `(book_id, field)` pair, the winner is the
//! highest-confidence non-null candidate; ties broken by most
//! recent `recorded_at`. The winner gets `is_winner = 1`, all
//! others for that field get `is_winner = 0`, and the value is
//! written to the corresponding `books` column.
//!
//! # Fields handled
//!
//! Direct text/numeric promotions only:
//! - `title`            → `books.title`
//! - `subtitle`         → `books.subtitle`
//! - `description`      → `books.description`
//! - `language`         → `books.language`
//! - `release_date`     → `books.release_date`
//! - `duration_seconds` → `books.duration_ms` (× 1000)
//!
//! Junction-table fields (author, narrator, publisher) require
//! lookup + insert into their identity tables and are handled by
//! a separate "resolve-identity" slice.
//!
//! # Idempotency
//!
//! Re-running consensus on the same book is a no-op when the
//! winners haven't changed. When new candidates arrive (e.g. an
//! Audnexus enrich completes after read-tags), the next consensus
//! pass picks up the higher-confidence row and updates `books`
//! accordingly.

use async_trait::async_trait;

use ab_core::{BookId, Error, Field, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Stage that picks winning candidates per field and updates `books`.
pub struct ConsensusStage;

impl ConsensusStage {
    /// Construct. No configuration needed at this layer — the
    /// winner-picking rule is structural, not policy.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for ConsensusStage {
    fn default() -> Self {
        Self::new()
    }
}

/// Fields that consensus knows how to promote via direct text
/// copy. Order is the iteration order at run time. Each entry's
/// target column comes from `Field::books_column()` — no separate
/// `target_column` literal here, so the two pieces of information
/// can't drift (slice C5.2 collapsed the `PromotableField` struct).
///
/// `DurationSeconds` is intentionally **not** in this list — it
/// needs the × 1000 integer transform handled by
/// `promote_duration`. `Genre` is also intentionally absent — it's
/// multi-value through the `book_genre` junction, handled by
/// `promote_genres`. Junction / resolve-identity fields
/// (`Author`, `Narrator`, `Publisher`, `Series`) return
/// `Field::books_column() == None` and live in `resolve-identity`.
const PROMOTABLE_FIELDS: &[Field] = &[
    Field::Title,
    Field::Subtitle,
    Field::Description,
    Field::Language,
    Field::ReleaseDate,
];

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("promote-consensus");

#[async_trait]
impl Stage for ConsensusStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // enrich-from-audnexus is the highest-confidence source. By
        // requiring it, consensus runs after both read-tags (which
        // enrich-from-audnexus requires) and enrich-from-audnexus. Adding
        // more enrichers later: extend this list so consensus
        // waits for them.
        &[crate::enrich::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let mut tx = ctx
            .library
            .pool()
            .begin()
            .await
            .map_err(|e| Error::Database(format!("consensus tx begin: {e}")))?;

        let mut updates = 0;
        for &field in PROMOTABLE_FIELDS {
            if promote_text_field(&mut tx, book_id, field).await? {
                updates += 1;
            }
        }
        if promote_duration(&mut tx, book_id).await? {
            updates += 1;
        }
        if promote_genres(&mut tx, book_id).await? {
            updates += 1;
        }
        // Slice 10A: clean ": Subtitle" suffix out of books.title
        // when books.subtitle is populated and matches. Conservative
        // rule keeps legitimate colon-containing titles
        // ("The 4-Hour Workweek: Escape 9-5") intact when no
        // subtitle field is set.
        if clean_title_subtitle(&mut tx, book_id).await? {
            updates += 1;
        }

        tx.commit()
            .await
            .map_err(|e| Error::Database(format!("consensus tx commit: {e}")))?;

        tracing::debug!(
            book = %book_id,
            updates,
            "consensus.done"
        );
        Ok(if updates > 0 {
            StageOutcome::Done
        } else {
            StageOutcome::Skipped
        })
    }
}

/// Pick the winning provenance row for `field`, set `is_winner`
/// flags, and write the value to the `books.<col>` column named
/// by `field.books_column()`. Returns `true` if any change was
/// made.
///
/// Panics on a `Field` whose `books_column()` returns `None` —
/// the invariant is that `PROMOTABLE_FIELDS` only contains
/// scalar-direct-copy variants. A non-PROMOTABLE field reaching
/// this fn is a programmer error.
async fn promote_text_field(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: Field,
) -> Result<bool> {
    let id = book_id.0;
    let field_str = field.as_str();
    let Some(target_column) = field.books_column() else {
        // Invariant: PROMOTABLE_FIELDS contains only fields whose
        // `books_column()` returns `Some`. A `None` here is a
        // programmer error (e.g. someone added `Field::Author`
        // to the const). Surface it as a stage error so it
        // can't slip past in production.
        return Err(Error::stage(
            STAGE_ID.as_str(),
            format!("PROMOTABLE_FIELDS contains {field} which has no books_column()"),
        ));
    };

    let winner = sqlx::query!(
        r#"SELECT provenance_id AS "provenance_id!", value
           FROM book_field_provenance
           WHERE book_id = ? AND field = ? AND value IS NOT NULL
           ORDER BY confidence DESC, recorded_at DESC
           LIMIT 1"#,
        id,
        field_str,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus pick {field}: {e}")))?;

    let Some(winner) = winner else {
        return Ok(false);
    };
    let Some(value) = winner.value else {
        // Defensive: the WHERE clause filters nulls, but Option
        // collapses can leak through if the query is rewritten
        // later. Treat as "no winner".
        return Ok(false);
    };

    // Set is_winner flags. Clear losers first, then set the
    // winner — otherwise a re-run that re-picks the same row
    // would briefly clear it.
    sqlx::query!(
        "UPDATE book_field_provenance \
         SET is_winner = 0 \
         WHERE book_id = ? AND field = ? AND provenance_id != ?",
        id,
        field_str,
        winner.provenance_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus clear losers {field}: {e}")))?;
    sqlx::query!(
        "UPDATE book_field_provenance SET is_winner = 1 WHERE provenance_id = ?",
        winner.provenance_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus set winner {field}: {e}")))?;

    // Write to the books column. `target_column` is a
    // `&'static str` returned by `Field::books_column()` — never
    // user input — so the `format!()` here is safe from injection.
    // (The `sqlx::query!` macros require literal SQL; for this
    // small fixed dispatch a runtime query is the pragmatic
    // shape. The closed `PROMOTABLE_FIELDS` list × the closed
    // `Field` enum × `Field::books_column()` is the implicit
    // allowlist.)
    let sql = format!("UPDATE books SET {target_column} = ? WHERE book_id = ?");
    sqlx::query(&sql)
        .bind(&value)
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus write books.{target_column}: {e}")))?;
    Ok(true)
}

/// Special case: `duration_seconds` (text) → `books.duration_ms`
/// (integer). Parse the value, multiply by 1000.
async fn promote_duration(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
) -> Result<bool> {
    let id = book_id.0;
    let field_str = Field::DurationSeconds.as_str();
    let winner = sqlx::query!(
        r#"SELECT provenance_id AS "provenance_id!", value
           FROM book_field_provenance
           WHERE book_id = ? AND field = ? AND value IS NOT NULL
           ORDER BY confidence DESC, recorded_at DESC
           LIMIT 1"#,
        id,
        field_str,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus pick duration_seconds: {e}")))?;

    let Some(winner) = winner else {
        return Ok(false);
    };
    let Some(text) = winner.value else {
        return Ok(false);
    };
    let Ok(secs) = text.parse::<i64>() else {
        tracing::warn!(
            book = %book_id,
            value = %text,
            "consensus.duration_parse_failed"
        );
        return Ok(false);
    };
    let ms = secs.saturating_mul(1000);

    sqlx::query!(
        "UPDATE book_field_provenance \
         SET is_winner = 0 \
         WHERE book_id = ? AND field = ? AND provenance_id != ?",
        id,
        field_str,
        winner.provenance_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus clear duration losers: {e}")))?;
    sqlx::query!(
        "UPDATE book_field_provenance SET is_winner = 1 WHERE provenance_id = ?",
        winner.provenance_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus set duration winner: {e}")))?;
    sqlx::query!("UPDATE books SET duration_ms = ? WHERE book_id = ?", ms, id,)
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus write duration_ms: {e}")))?;
    Ok(true)
}

/// Multi-value promotion: every distinct `value` for
/// `field='genre'` becomes a row in the `book_genre` junction
/// table. Unlike the scalar promoters above, "winning" here
/// applies per unique value — the highest-confidence row for
/// each canonical genre slug gets `is_winner=1`, with that
/// confidence carried into `book_genre.confidence`.
///
/// Side-effect: ensures the `genres` table has a row for each
/// canonical slug we're inserting (auto-create on first use).
/// Display name comes from `genre_code::display_name(slug, "en")`
/// — denormalised English form stored once; the locale-aware
/// display happens at read time.
///
/// Removes `book_genre` rows whose slug no longer has any
/// provenance candidate (e.g. a misclassified candidate that
/// got deleted).
async fn promote_genres(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
) -> Result<bool> {
    let id = book_id.0;
    let genre_field = Field::Genre.as_str();
    // Distinct genre slugs + their max confidence + winning
    // provenance_id (the row that supplied the max confidence).
    // The subquery's `field = ?` repeats the outer bind — sqlite
    // doesn't allow positional reuse, so the parameter is bound
    // twice.
    let rows = sqlx::query!(
        r#"SELECT value AS "value!",
                  MAX(confidence) AS "best_confidence!: f64",
                  (SELECT provenance_id FROM book_field_provenance p2
                   WHERE p2.book_id = book_field_provenance.book_id
                     AND p2.field = ?
                     AND p2.value = book_field_provenance.value
                   ORDER BY p2.confidence DESC, p2.recorded_at DESC
                   LIMIT 1) AS "winner_id!"
           FROM book_field_provenance
           WHERE book_id = ? AND field = ? AND value IS NOT NULL
           GROUP BY value"#,
        genre_field,
        id,
        genre_field,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus pick genres: {e}")))?;

    if rows.is_empty() {
        return Ok(false);
    }

    // Clear all is_winner flags for genre rows, then set the
    // winners. Same shape as the scalar promoters; safe order
    // because all happens in the same tx.
    sqlx::query!(
        "UPDATE book_field_provenance SET is_winner = 0 \
         WHERE book_id = ? AND field = ?",
        id,
        genre_field,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus clear genre losers: {e}")))?;

    let mut current_slugs: Vec<String> = Vec::with_capacity(rows.len());
    for row in &rows {
        let slug = &row.value;
        let confidence = row.best_confidence;
        let winner_id = row.winner_id;
        current_slugs.push(slug.clone());

        // Set winner flag.
        sqlx::query!(
            "UPDATE book_field_provenance SET is_winner = 1 WHERE provenance_id = ?",
            winner_id,
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus set genre winner: {e}")))?;

        // Ensure `genres` row exists for this slug. The English
        // display-name is stored once at first-insert; locale-
        // aware rendering happens at read time via
        // `genre_code::display_name(slug, locale)`.
        let display = ab_core::genre_code::display_name(slug, "en");
        sqlx::query!(
            "INSERT OR IGNORE INTO genres (canonical_id, display_name) VALUES (?, ?)",
            slug,
            display,
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus upsert genre row: {e}")))?;

        // Resolve the genre_id (separate query — sqlite's
        // INSERT-OR-IGNORE doesn't return the existing id on
        // conflict).
        let genre_row = sqlx::query!(
            r#"SELECT genre_id AS "genre_id!" FROM genres WHERE canonical_id = ?"#,
            slug,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus lookup genre_id: {e}")))?;

        sqlx::query!(
            "INSERT INTO book_genre (book_id, genre_id, confidence) \
             VALUES (?, ?, ?) \
             ON CONFLICT(book_id, genre_id) DO UPDATE SET confidence = excluded.confidence",
            id,
            genre_row.genre_id,
            confidence,
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus upsert book_genre: {e}")))?;
    }

    // Clean up `book_genre` rows whose slug no longer has any
    // candidate. Builds a NOT-IN clause from the current slug
    // list — dynamic SQL, but the slugs themselves come from
    // the closed `book_field_provenance.value` namespace
    // (written by normalize() in read-tags / Audnexus) so
    // injection isn't a concern. Bound parameters anyway for
    // safety.
    let placeholders = std::iter::repeat_n("?", current_slugs.len())
        .collect::<Vec<_>>()
        .join(",");
    let cleanup_sql = format!(
        "DELETE FROM book_genre \
         WHERE book_id = ? AND genre_id NOT IN ( \
             SELECT genre_id FROM genres WHERE canonical_id IN ({placeholders}) \
         )"
    );
    let mut q = sqlx::query(&cleanup_sql).bind(id);
    for slug in &current_slugs {
        q = q.bind(slug);
    }
    q.execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus cleanup book_genre: {e}")))?;

    Ok(true)
}

/// Clean a `": Subtitle"` suffix out of `books.title` when
/// `books.subtitle` already holds the same suffix (slice 10A).
///
/// Conservative rule: strip only when both fields are populated
/// AND the colon-suffix in title matches the subtitle column. This
/// avoids touching titles like "The 4-Hour Workweek: Escape 9-5"
/// where the subtitle column is empty — the colon there is part
/// of the title proper.
///
/// Returns `true` if any change was made; `false` is the
/// idempotent re-run case (title was already clean).
async fn clean_title_subtitle(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
) -> Result<bool> {
    let id = book_id.0;
    let row = sqlx::query!("SELECT title, subtitle FROM books WHERE book_id = ?", id,)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus title-subtitle read: {e}")))?;
    let Some(row) = row else {
        return Ok(false);
    };
    let Some(subtitle) = row.subtitle.as_deref() else {
        return Ok(false);
    };
    let subtitle_trim = subtitle.trim();
    if subtitle_trim.is_empty() {
        return Ok(false);
    }
    let Some(cleaned) = strip_subtitle_suffix(&row.title, subtitle_trim) else {
        return Ok(false);
    };
    sqlx::query!("UPDATE books SET title = ? WHERE book_id = ?", cleaned, id,)
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("consensus title-subtitle write: {e}")))?;
    tracing::info!(
        book = %book_id,
        old_title = %row.title,
        new_title = %cleaned,
        "consensus.title.subtitle_stripped"
    );
    Ok(true)
}

/// Pure-text helper: when `title` ends with `": <subtitle>"` (or
/// `" - <subtitle>"`), return the prefix; else `None`.
/// Case-insensitive match on the suffix. Whitespace around the
/// separator is permissive (`":  Subtitle"`, `" - Subtitle"`).
fn strip_subtitle_suffix(title: &str, subtitle: &str) -> Option<String> {
    for delim in [":", " - "] {
        if let Some(idx) = title.rfind(delim) {
            let after_idx = idx + delim.len();
            let after = title[after_idx..].trim_start();
            if after.eq_ignore_ascii_case(subtitle) {
                let prefix = title[..idx].trim_end();
                if !prefix.is_empty() {
                    return Some(prefix.to_owned());
                }
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    use super::*;

    async fn fresh_db(dir: &std::path::Path) -> ab_db::LibraryDb {
        let path = dir.join("library.db");
        ab_db::LibraryDb::open(&path, &DbTunables::default())
            .await
            .expect("open db")
    }

    async fn fresh_ctx(dir: &std::path::Path) -> StageContext {
        let lib = fresh_db(dir).await;
        let eph = ab_db::EphemeralDb::open(&dir.join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        StageContext {
            library: lib,
            ephemeral: eph,
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: "promote-consensus",
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let stage = ConsensusStage::new();
        assert_eq!(stage.name(), "promote-consensus");
        assert_eq!(stage.requires(), &[crate::enrich::STAGE_ID]);
    }

    #[tokio::test]
    async fn picks_highest_confidence_title_and_promotes_to_books() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;

        // Seed: one book, two title candidates of different
        // confidence.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'title', 'tag-value',      'tag_file',         'read-tags',        0.7), \
                    (1, 'title', 'audnexus-value', 'audnexus_asin_us', 'enrich-from-audnexus', 0.95)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        let stage = ConsensusStage::new();
        let outcome = stage.run(&ctx, BookId(1)).await.expect("consensus run");
        assert_eq!(outcome, StageOutcome::Done);

        let title: String = sqlx::query_scalar("SELECT title FROM books WHERE book_id = 1")
            .fetch_one(ctx.library.pool())
            .await
            .expect("read books.title");
        assert_eq!(title, "audnexus-value");

        let winners: Vec<(String, i64)> = sqlx::query_as(
            "SELECT value, is_winner FROM book_field_provenance \
             WHERE book_id = 1 AND field = 'title' ORDER BY confidence DESC",
        )
        .fetch_all(ctx.library.pool())
        .await
        .expect("read provenance");
        assert_eq!(winners.len(), 2);
        assert_eq!(winners[0].0, "audnexus-value");
        assert_eq!(winners[0].1, 1, "high-confidence row is winner");
        assert_eq!(winners[1].0, "tag-value");
        assert_eq!(winners[1].1, 0, "low-confidence row is loser");
    }

    #[tokio::test]
    async fn promotes_duration_seconds_to_duration_ms() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'duration_seconds', '36000', 'audnexus_asin_us', 'enrich-from-audnexus', 0.95)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        let stage = ConsensusStage::new();
        stage.run(&ctx, BookId(1)).await.expect("consensus run");

        let ms: Option<i64> = sqlx::query_scalar("SELECT duration_ms FROM books WHERE book_id = 1")
            .fetch_one(ctx.library.pool())
            .await
            .expect("read duration_ms");
        assert_eq!(ms, Some(36_000_000_i64), "10 hours in ms");
    }

    #[tokio::test]
    async fn skips_when_no_candidates_exist() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");

        let stage = ConsensusStage::new();
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[test]
    fn strip_subtitle_suffix_colon_separator() {
        assert_eq!(
            strip_subtitle_suffix("Foundation: Empire", "Empire"),
            Some("Foundation".to_owned())
        );
    }

    #[test]
    fn strip_subtitle_suffix_dash_separator() {
        assert_eq!(
            strip_subtitle_suffix(
                "The Way of Kings - Stormlight Archive Book 1",
                "Stormlight Archive Book 1"
            ),
            Some("The Way of Kings".to_owned())
        );
    }

    #[test]
    fn strip_subtitle_suffix_case_insensitive() {
        assert_eq!(
            strip_subtitle_suffix("Mistborn: the Final empire", "The Final Empire"),
            Some("Mistborn".to_owned())
        );
    }

    #[test]
    fn strip_subtitle_suffix_extra_whitespace_after_separator() {
        assert_eq!(
            strip_subtitle_suffix("Title:    Sub", "Sub"),
            Some("Title".to_owned())
        );
    }

    #[test]
    fn strip_subtitle_suffix_no_match_returns_none() {
        // Colon present but suffix doesn't match — leave as is.
        assert_eq!(
            strip_subtitle_suffix("The 4-Hour Workweek: Escape 9-5", "Some Other Subtitle"),
            None
        );
        // No separator at all.
        assert_eq!(
            strip_subtitle_suffix("The Final Empire", "The Final Empire"),
            None
        );
    }

    #[test]
    fn strip_subtitle_suffix_empty_prefix_rejected() {
        // ": Subtitle" alone — would leave empty title; refuse.
        assert_eq!(strip_subtitle_suffix(": Subtitle", "Subtitle"), None);
    }

    #[tokio::test]
    async fn consensus_strips_redundant_subtitle_from_title() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        // Seed a book whose title already includes the subtitle.
        sqlx::query(
            "INSERT INTO books (book_id, title, subtitle) VALUES \
                 (1, 'Foundation: Empire', 'Empire')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed book");
        // Need at least one provenance row so the consensus stage
        // reports `Done` (any column will do).
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'description', 'desc', 'audnexus_asin_us', 'enrich-from-audnexus', 0.95)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        let stage = ConsensusStage::new();
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Done);

        let title: String = sqlx::query_scalar("SELECT title FROM books WHERE book_id = 1")
            .fetch_one(ctx.library.pool())
            .await
            .expect("read title");
        assert_eq!(title, "Foundation", "subtitle suffix stripped");
    }
}
