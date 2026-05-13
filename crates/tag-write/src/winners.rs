//! Winner-row selection from `book_field_provenance`.
//!
//! The consensus stage marks at most one row per `(book_id,
//! field)` as `is_winner = 1`. Both tag-write stages read those
//! rows to know what value to embed on disk.
//!
//! The `field` column carries a closed set of strings — see
//! [`ab_core::Field`]. This module returns the row as
//! `(Field, value, source)` so callers can match on the typed
//! enum and consult [`crate::skip_for_final_pass`] on the source
//! string in one step.

use ab_core::{Error, Field, Result};
use sqlx::SqlitePool;

/// One winning provenance row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldWinner {
    /// Which `book_field_provenance.field` the row represents.
    /// Closed enum — the SELECT filters out any string that
    /// doesn't parse via [`Field::parse`], which protects
    /// against forward-compat drift if a migration ever widens
    /// the `CHECK` set without updating the enum.
    pub field: Field,
    /// `book_field_provenance.value` — text representation of
    /// the winner. May be empty / absent; the on-disk writer
    /// treats `None` as "clear this tag", but persistence shape
    /// is the writer's concern, not this module's.
    pub value: Option<String>,
    /// `book_field_provenance.source` — free-form text. Compare
    /// to [`crate::USER_EDIT_SOURCE`] for the late-stage skip.
    pub source: String,
}

/// Pull every `is_winner = 1` row for one book, ordered by
/// `Field` for stable test output.
///
/// Rows whose `field` column doesn't parse to a known
/// [`Field`] variant are silently dropped — the consensus
/// stage's writers go through the typed enum (via
/// [`Field::parse`]) so a foreign value can only arrive via
/// a hand-written migration mismatch. Logging is at `debug`
/// so an operator can grep when chasing that exact bug class.
///
/// # Errors
///
/// Returns [`Error::Database`] on SQLite failure.
pub async fn select_winners_for_book(pool: &SqlitePool, book_id: i64) -> Result<Vec<FieldWinner>> {
    let rows = sqlx::query!(
        r#"
        SELECT  field   AS "field!: String",
                value   AS "value: String",
                source  AS "source!: String"
          FROM book_field_provenance
         WHERE book_id = ? AND is_winner = 1
         ORDER BY field
        "#,
        book_id,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| Error::Database(format!("select_winners_for_book: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if let Some(field) = Field::parse(&r.field) {
            out.push(FieldWinner {
                field,
                value: r.value,
                source: r.source,
            });
        } else {
            tracing::debug!(
                book_id,
                field = %r.field,
                "tag-write.winners.unknown_field"
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::LibraryDb;
    use tempfile::TempDir;

    async fn fresh() -> (LibraryDb, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open");
        (library, tmp)
    }

    async fn seed_book(library: &LibraryDb, book_id: i64) {
        sqlx::query("INSERT INTO books (book_id, title) VALUES (?, 'fixture')")
            .bind(book_id)
            .execute(library.pool())
            .await
            .expect("seed book");
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "test seed helper mirrors book_field_provenance schema columns"
    )]
    async fn seed_winner(
        library: &LibraryDb,
        book_id: i64,
        field: &str,
        value: &str,
        source: &str,
        is_winner: i64,
    ) {
        sqlx::query(
            "INSERT INTO book_field_provenance \
                 (book_id, field, value, source, stage, confidence, is_winner) \
             VALUES (?, ?, ?, ?, 'test-stage', 0.9, ?)",
        )
        .bind(book_id)
        .bind(field)
        .bind(value)
        .bind(source)
        .bind(is_winner)
        .execute(library.pool())
        .await
        .expect("seed provenance");
    }

    #[tokio::test]
    async fn returns_only_winner_rows() {
        let (library, _tmp) = fresh().await;
        seed_book(&library, 1).await;
        seed_winner(&library, 1, "title", "Foundation", "audible-search", 1).await;
        seed_winner(&library, 1, "title", "Foundation, Vol. 1", "tag_file", 0).await;
        seed_winner(&library, 1, "author", "Asimov", "audnexus-enrich", 1).await;

        let winners = select_winners_for_book(library.pool(), 1)
            .await
            .expect("select");
        assert_eq!(winners.len(), 2);
        // ORDER BY field — author < title alphabetically.
        assert_eq!(winners[0].field, Field::Author);
        assert_eq!(winners[0].value.as_deref(), Some("Asimov"));
        assert_eq!(winners[1].field, Field::Title);
        assert_eq!(winners[1].value.as_deref(), Some("Foundation"));
    }

    #[tokio::test]
    async fn returns_empty_for_unknown_book() {
        let (library, _tmp) = fresh().await;
        let winners = select_winners_for_book(library.pool(), 999)
            .await
            .expect("select");
        assert!(winners.is_empty());
    }

    #[tokio::test]
    async fn propagates_user_edit_source_verbatim() {
        let (library, _tmp) = fresh().await;
        seed_book(&library, 1).await;
        seed_winner(&library, 1, "title", "User Title", "user_edit", 1).await;

        let winners = select_winners_for_book(library.pool(), 1)
            .await
            .expect("select");
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].source, "user_edit");
        // Pair with the predicate the late-stage uses.
        assert!(crate::skip_for_final_pass(&winners[0].source));
    }
}
