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
//! a separate "identity-resolve" slice.
//!
//! # Idempotency
//!
//! Re-running consensus on the same book is a no-op when the
//! winners haven't changed. When new candidates arrive (e.g. an
//! Audnexus enrich completes after tag-read), the next consensus
//! pass picks up the higher-confidence row and updates `books`
//! accordingly.

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageOutcome};

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

/// Fields that consensus knows how to promote directly into the
/// `books` table. Order is the iteration order at run time. The
/// `target_column` doubles as the SQL column name; it's safe as
/// long as we never accept user input here (this list is closed).
const PROMOTABLE_FIELDS: &[PromotableField] = &[
    PromotableField {
        provenance_field: "title",
        target_column: "title",
    },
    PromotableField {
        provenance_field: "subtitle",
        target_column: "subtitle",
    },
    PromotableField {
        provenance_field: "description",
        target_column: "description",
    },
    PromotableField {
        provenance_field: "language",
        target_column: "language",
    },
    PromotableField {
        provenance_field: "release_date",
        target_column: "release_date",
    },
];

struct PromotableField {
    /// `book_field_provenance.field` value.
    provenance_field: &'static str,
    /// Target column on `books`.
    target_column: &'static str,
}

#[async_trait]
impl Stage for ConsensusStage {
    fn name(&self) -> &'static str {
        "consensus"
    }

    fn requires(&self) -> &'static [&'static str] {
        // audnexus-enrich is the highest-confidence source. By
        // requiring it, consensus runs after both tag-read (which
        // audnexus-enrich requires) and audnexus-enrich. Adding
        // more enrichers later: extend this list so consensus
        // waits for them.
        &["audnexus-enrich"]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let mut tx = ctx
            .library
            .pool()
            .begin()
            .await
            .map_err(|e| Error::Database(format!("consensus tx begin: {e}")))?;

        let mut updates = 0;
        for field in PROMOTABLE_FIELDS {
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
/// flags, and write the value to `books.<target_column>`. Returns
/// `true` if any change was made.
async fn promote_text_field(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: &PromotableField,
) -> Result<bool> {
    let id = book_id.0;
    let provenance_field = field.provenance_field;
    let winner = sqlx::query!(
        r#"SELECT provenance_id AS "provenance_id!", value
           FROM book_field_provenance
           WHERE book_id = ? AND field = ? AND value IS NOT NULL
           ORDER BY confidence DESC, recorded_at DESC
           LIMIT 1"#,
        id,
        provenance_field,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus pick {provenance_field}: {e}")))?;

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
        provenance_field,
        winner.provenance_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus clear losers {provenance_field}: {e}")))?;
    sqlx::query!(
        "UPDATE book_field_provenance SET is_winner = 1 WHERE provenance_id = ?",
        winner.provenance_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("consensus set winner {provenance_field}: {e}")))?;

    // Write to the books column. Column name is a `&'static str` from
    // PROMOTABLE_FIELDS — never user input — so the format!() here
    // is safe from injection. (The macros require literal SQL; for
    // this small fixed dispatch a runtime query is the pragmatic
    // shape. The closed `PROMOTABLE_FIELDS` list is the implicit
    // allowlist.)
    let sql = format!(
        "UPDATE books SET {} = ? WHERE book_id = ?",
        field.target_column
    );
    sqlx::query(&sql)
        .bind(&value)
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(|e| {
            Error::Database(format!(
                "consensus write books.{}: {e}",
                field.target_column
            ))
        })?;
    Ok(true)
}

/// Special case: `duration_seconds` (text) → `books.duration_ms`
/// (integer). Parse the value, multiply by 1000.
async fn promote_duration(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
) -> Result<bool> {
    let id = book_id.0;
    let winner = sqlx::query!(
        r#"SELECT provenance_id AS "provenance_id!", value
           FROM book_field_provenance
           WHERE book_id = ? AND field = 'duration_seconds' AND value IS NOT NULL
           ORDER BY confidence DESC, recorded_at DESC
           LIMIT 1"#,
        id,
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
         WHERE book_id = ? AND field = 'duration_seconds' AND provenance_id != ?",
        id,
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
    // Distinct genre slugs + their max confidence + winning
    // provenance_id (the row that supplied the max confidence).
    let rows = sqlx::query!(
        r#"SELECT value AS "value!",
                  MAX(confidence) AS "best_confidence!: f64",
                  (SELECT provenance_id FROM book_field_provenance p2
                   WHERE p2.book_id = book_field_provenance.book_id
                     AND p2.field = 'genre'
                     AND p2.value = book_field_provenance.value
                   ORDER BY p2.confidence DESC, p2.recorded_at DESC
                   LIMIT 1) AS "winner_id!"
           FROM book_field_provenance
           WHERE book_id = ? AND field = 'genre' AND value IS NOT NULL
           GROUP BY value"#,
        id,
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
         WHERE book_id = ? AND field = 'genre'",
        id,
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
    // (written by normalize() in tag-read / Audnexus) so
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
            stage_name: "consensus",
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let stage = ConsensusStage::new();
        assert_eq!(stage.name(), "consensus");
        assert_eq!(stage.requires(), &["audnexus-enrich"]);
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
             (book_id, field, value, source, confidence) \
             VALUES (1, 'title', 'tag-value', 'tag_file', 0.7), \
                    (1, 'title', 'audnexus-value', 'audnexus_asin_us', 0.95)",
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
             (book_id, field, value, source, confidence) \
             VALUES (1, 'duration_seconds', '36000', 'audnexus_asin_us', 0.95)",
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
}
