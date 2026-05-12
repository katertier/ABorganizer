//! Identity-resolve stage.
//!
//! Promotes author / narrator / publisher candidates from
//! `book_field_provenance` into the identity tables (`authors`,
//! `narrators`, `publishers`) and links them to books via the FK
//! columns + `book_narrator` junction.
//!
//! # Why a separate stage
//!
//! The `consensus` stage handles direct text/numeric promotions
//! (`title`, `subtitle`, etc.) — fields where the candidate value
//! lands directly in a `books` column. Author / narrator / publisher
//! need lookup-or-insert into the identity tables first, and
//! narrator is multi-valued (handled via the `book_narrator`
//! junction). Different shape, separate stage.
//!
//! # Promotion rules
//!
//! - **`author`**: pick the highest-confidence candidate per book,
//!   find-or-insert into `authors` by name (case-insensitive),
//!   set `books.author_id`.
//! - **`publisher`**: same shape as author, into `publishers`,
//!   set `books.publisher_id`. `publishers.name` is `UNIQUE` so the
//!   find step is a direct lookup.
//! - **`narrator`**: take ALL distinct non-null candidates (not just
//!   the winner — books typically have multiple narrators).
//!   find-or-insert each into `narrators`. Rewrite
//!   `book_narrator` to exactly the discovered set (clear-then-add
//!   pattern so re-runs converge cleanly).
//!
//! # Match precedence
//!
//! 1. **By Audnexus ASIN** (when the provenance row carries an
//!    `external_id`): query `authors.audible_id` / `narrators.audible_id`.
//!    Wins when the catalog has spelled the contributor name
//!    differently between books (different romanisation,
//!    "First Last" vs "Last, First", etc.) but the ASIN matches.
//! 2. **By case-insensitive name** (fallback): `lower(name)`
//!    match. Wins for sources with no canonical id (tag-read).
//!
//! When inserting a new row that has an ASIN, the ASIN is stored
//! in `audible_id` so subsequent runs benefit from the unique
//! partial index. When a name-match wins on a row that's missing
//! `audible_id` and the current candidate has one, the existing
//! row is updated to fill it in.

use std::collections::HashSet;

use async_trait::async_trait;

use ab_core::{BookId, Error, Field, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Stage that resolves identities (authors/narrators/publishers).
pub struct IdentityResolveStage;

impl IdentityResolveStage {
    /// Construct. No tunables — the promotion rules are structural.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for IdentityResolveStage {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed identifier for this stage.
pub const STAGE_ID: StageId = StageId::new("identity-resolve");

#[async_trait]
impl Stage for IdentityResolveStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // After consensus runs so the direct-column promotions are
        // already in place. Consensus depends on audnexus-enrich,
        // which writes the contributor candidates this stage needs;
        // transitivity covers the ordering.
        &[crate::consensus::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let mut tx = ctx
            .library
            .pool()
            .begin()
            .await
            .map_err(|e| Error::Database(format!("identity tx begin: {e}")))?;

        let mut updates = 0;
        if resolve_author(&mut tx, book_id).await? {
            updates += 1;
        }
        if resolve_publisher(&mut tx, book_id).await? {
            updates += 1;
        }
        let narrator_count = resolve_narrators(&mut tx, book_id).await?;
        if narrator_count > 0 {
            updates += 1;
        }

        tx.commit()
            .await
            .map_err(|e| Error::Database(format!("identity tx commit: {e}")))?;

        tracing::debug!(
            book = %book_id,
            updates,
            narrator_count,
            "identity.resolve.done"
        );
        Ok(if updates > 0 {
            StageOutcome::Done
        } else {
            StageOutcome::Skipped
        })
    }
}

/// Pick the winning author candidate, find-or-insert into
/// `authors`, set `books.author_id`. Returns `true` if any change.
async fn resolve_author(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
) -> Result<bool> {
    let Some(candidate) = pick_winner_with_id(tx, book_id, Field::Author).await? else {
        return Ok(false);
    };
    let author_id = find_or_insert_person(
        tx,
        "authors",
        "author_id",
        &candidate.name,
        candidate.external_id.as_deref(),
    )
    .await?;
    let id = book_id.0;
    sqlx::query!(
        "UPDATE books SET author_id = ? WHERE book_id = ?",
        author_id,
        id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity write books.author_id: {e}")))?;
    Ok(true)
}

/// Same shape as `resolve_author` but for publishers. The
/// `publishers.name` UNIQUE constraint makes this branch's find
/// step a single direct lookup.
async fn resolve_publisher(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
) -> Result<bool> {
    let Some(name) = pick_winner_name_only(tx, book_id, Field::Publisher).await? else {
        return Ok(false);
    };
    let publisher_id = find_or_insert_publisher(tx, &name).await?;
    let id = book_id.0;
    sqlx::query!(
        "UPDATE books SET publisher_id = ? WHERE book_id = ?",
        publisher_id,
        id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity write books.publisher_id: {e}")))?;
    Ok(true)
}

/// Take every distinct narrator candidate for this book, find-or-
/// insert each into `narrators`, then overwrite `book_narrator` to
/// exactly that set. Returns the count of narrators linked.
async fn resolve_narrators(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
) -> Result<usize> {
    let candidates = fetch_all_distinct(tx, book_id, Field::Narrator).await?;
    if candidates.is_empty() {
        return Ok(0);
    }

    // Resolve every name to an id first so we have a clean target
    // set before touching the junction.
    let mut narrator_ids: Vec<i64> = Vec::with_capacity(candidates.len());
    for c in &candidates {
        let nid = find_or_insert_person(
            tx,
            "narrators",
            "narrator_id",
            &c.name,
            c.external_id.as_deref(),
        )
        .await?;
        narrator_ids.push(nid);
    }

    // Clear-then-add. SQLite has no ON CONFLICT REPLACE shape that
    // both inserts NEW rows and removes STALE rows in one step; the
    // junction is small enough (typically 1–3 narrators per book)
    // that the simple delete-then-insert is fine.
    let id = book_id.0;
    sqlx::query!("DELETE FROM book_narrator WHERE book_id = ?", id)
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity clear book_narrator: {e}")))?;
    for nid in &narrator_ids {
        sqlx::query!(
            "INSERT INTO book_narrator (book_id, narrator_id) VALUES (?, ?)",
            id,
            nid,
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity insert book_narrator: {e}")))?;
    }
    Ok(narrator_ids.len())
}

/// One identity candidate: the trimmed name + an optional
/// external identifier (Audnexus ASIN, etc.) attached by the
/// source.
#[derive(Debug, Clone)]
struct IdentityCandidate {
    name: String,
    external_id: Option<String>,
}

/// Highest-confidence non-null candidate for `field`. Returns the
/// trimmed value (+ `external_id` if present) or `None` if no
/// candidate exists.
async fn pick_winner_with_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: Field,
) -> Result<Option<IdentityCandidate>> {
    let id = book_id.0;
    let field_str = field.as_str();
    let row = sqlx::query!(
        "SELECT value, external_id FROM book_field_provenance \
         WHERE book_id = ? AND field = ? AND value IS NOT NULL \
         ORDER BY confidence DESC, recorded_at DESC LIMIT 1",
        id,
        field_str,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity pick {field}: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    let Some(value) = row.value else {
        return Ok(None);
    };
    let name = value.trim().to_owned();
    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(IdentityCandidate {
        name,
        external_id: row
            .external_id
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
    }))
}

/// Same as `pick_winner_with_id` but for publishers (no `external_id`
/// support yet — Audnexus doesn't return one for publishers; the
/// shape is kept narrow until a source provides them).
async fn pick_winner_name_only(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: Field,
) -> Result<Option<String>> {
    Ok(pick_winner_with_id(tx, book_id, field)
        .await?
        .map(|c| c.name))
}

/// Every distinct non-null candidate for `field` across all
/// sources, normalised by trim + case-insensitive dedup on name.
/// Preserves the `external_id` from the highest-confidence row that
/// brought each name (`audnexus_asin_us` beats `tag_file`).
async fn fetch_all_distinct(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: Field,
) -> Result<Vec<IdentityCandidate>> {
    let id = book_id.0;
    let field_str = field.as_str();
    let rows = sqlx::query!(
        "SELECT value, external_id FROM book_field_provenance \
         WHERE book_id = ? AND field = ? AND value IS NOT NULL \
         ORDER BY confidence DESC, recorded_at DESC",
        id,
        field_str,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity fetch all {field}: {e}")))?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<IdentityCandidate> = Vec::new();
    for r in rows {
        let Some(v) = r.value else { continue };
        let trimmed = v.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(IdentityCandidate {
                name: trimmed.to_owned(),
                external_id: r
                    .external_id
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty()),
            });
        }
    }
    Ok(out)
}

/// Find an existing author / narrator, or insert a new row.
/// Returns the row's id.
///
/// Match precedence:
/// 1. `audible_id = external_id` (when `external_id` is `Some`).
/// 2. Case-insensitive name match.
///
/// When a row matches by name but has no `audible_id` and the
/// current candidate brings one, the existing row is updated to
/// fill it in (back-filling identity across enrichment runs).
///
/// `table` and `id_column` are `&'static str` from a closed call
/// site allowlist — never user input — so the runtime SQL via
/// `format!` is safe from injection here. (The macros require
/// string literals; for this two-table dispatch the runtime path
/// is the pragmatic shape.)
async fn find_or_insert_person(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    id_column: &'static str,
    name: &str,
    external_id: Option<&str>,
) -> Result<i64> {
    // 1) ASIN match wins when we have one and someone else has
    //    already inserted this person with it. The
    //    `idx_authors_audible` partial unique index makes this O(log n).
    if let Some(ext) = external_id {
        let select_by_id = format!(
            "SELECT {id_column} FROM {table} \
             WHERE audible_id = ? LIMIT 1"
        );
        let existing: Option<i64> = sqlx::query_scalar(&select_by_id)
            .bind(ext)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| Error::Database(format!("identity lookup-by-id {table}: {e}")))?;
        if let Some(id) = existing {
            return Ok(id);
        }
    }
    // 2) Case-insensitive name lookup.
    let select_sql = format!(
        "SELECT {id_column}, audible_id FROM {table} \
         WHERE lower(name) = lower(?) LIMIT 1"
    );
    let existing: Option<(i64, Option<String>)> = sqlx::query_as(&select_sql)
        .bind(name)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity lookup {table}: {e}")))?;
    if let Some((id, existing_id)) = existing {
        // Back-fill audible_id if the row has none and we have one
        // to offer.
        if existing_id.is_none() {
            if let Some(ext) = external_id {
                let update_sql = format!("UPDATE {table} SET audible_id = ? WHERE {id_column} = ?");
                sqlx::query(&update_sql)
                    .bind(ext)
                    .bind(id)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| Error::Database(format!("identity backfill {table}: {e}")))?;
            }
        }
        return Ok(id);
    }
    // 3) Fresh insert. Carries the audible_id when present.
    let insert_sql =
        format!("INSERT INTO {table} (name, audible_id) VALUES (?, ?) RETURNING {id_column}");
    let new_id: i64 = sqlx::query_scalar(&insert_sql)
        .bind(name)
        .bind(external_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity insert {table}: {e}")))?;
    Ok(new_id)
}

/// Publishers have `name TEXT NOT NULL UNIQUE`, so the find step
/// is a direct unique-key lookup. Separated from
/// `find_or_insert_person` so the unique-constraint conflict path
/// (e.g. case-difference collisions) is explicit.
async fn find_or_insert_publisher(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    name: &str,
) -> Result<i64> {
    let existing: Option<i64> = sqlx::query_scalar!(
        "SELECT publisher_id FROM publishers WHERE lower(name) = lower(?) LIMIT 1",
        name,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity lookup publishers: {e}")))?
    .flatten();
    if let Some(id) = existing {
        return Ok(id);
    }
    let new_id: i64 = sqlx::query_scalar!(
        "INSERT INTO publishers (name) VALUES (?) RETURNING publisher_id",
        name,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity insert publishers: {e}")))?;
    Ok(new_id)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use ab_core::tunables::DbTunables;
    use tempfile::TempDir;

    use super::*;

    async fn fresh_ctx(dir: &std::path::Path) -> StageContext {
        let lib = ab_db::LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let eph = ab_db::EphemeralDb::open(&dir.join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        StageContext {
            library: lib,
            ephemeral: eph,
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: "identity-resolve",
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let stage = IdentityResolveStage::new();
        assert_eq!(stage.name(), "identity-resolve");
        assert_eq!(stage.requires(), &[crate::consensus::STAGE_ID]);
    }

    #[tokio::test]
    async fn resolves_author_and_publisher_and_narrators() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, confidence) \
             VALUES \
               (1, 'author', 'Brandon Sanderson', 'audnexus_asin_us', 0.95), \
               (1, 'author', 'brandon sanderson', 'tag_file', 0.7), \
               (1, 'publisher', 'Recorded Books', 'audnexus_asin_us', 0.95), \
               (1, 'narrator', 'Michael Kramer', 'audnexus_asin_us', 0.95), \
               (1, 'narrator', 'Kate Reading',   'audnexus_asin_us', 0.95)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        let stage = IdentityResolveStage::new();
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Done);

        // Author: exactly one row, "Brandon Sanderson" (the
        // case-insensitive dedup picks the higher-confidence
        // capitalisation).
        let author_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM authors")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count authors");
        assert_eq!(author_count, 1);
        let author_name: String = sqlx::query_scalar("SELECT name FROM authors LIMIT 1")
            .fetch_one(ctx.library.pool())
            .await
            .expect("read author");
        assert_eq!(author_name, "Brandon Sanderson");
        let book_author: Option<i64> =
            sqlx::query_scalar("SELECT author_id FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("read books.author_id");
        assert!(book_author.is_some(), "books.author_id linked");

        // Publisher: one row.
        let publisher_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM publishers")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count publishers");
        assert_eq!(publisher_count, 1);
        let book_publisher: Option<i64> =
            sqlx::query_scalar("SELECT publisher_id FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("read books.publisher_id");
        assert!(book_publisher.is_some());

        // Narrators: two distinct rows, both linked.
        let narrator_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narrators")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count narrators");
        assert_eq!(narrator_count, 2);
        let link_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM book_narrator WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("count links");
        assert_eq!(link_count, 2);
    }

    #[tokio::test]
    async fn rerun_replaces_narrator_set_idempotently() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, confidence) \
             VALUES (1, 'narrator', 'Original Reader', 'tag_file', 0.7)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed");

        let stage = IdentityResolveStage::new();
        stage.run(&ctx, BookId(1)).await.expect("first run");

        // Replace the narrator candidate with a different name.
        sqlx::query("DELETE FROM book_field_provenance WHERE book_id = 1 AND field = 'narrator'")
            .execute(ctx.library.pool())
            .await
            .expect("clear");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, confidence) \
             VALUES (1, 'narrator', 'New Reader', 'audnexus_asin_us', 0.95)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("replace");

        stage.run(&ctx, BookId(1)).await.expect("second run");

        let links: Vec<(i64, String)> = sqlx::query_as(
            "SELECT bn.narrator_id, n.name \
             FROM book_narrator bn JOIN narrators n ON n.narrator_id = bn.narrator_id \
             WHERE bn.book_id = 1",
        )
        .fetch_all(ctx.library.pool())
        .await
        .expect("read links");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "New Reader");
    }

    #[tokio::test]
    async fn skips_when_no_identity_candidates() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed");

        let stage = IdentityResolveStage::new();
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn audnexus_asin_overrides_name_match_across_books() {
        // Two books credit different romanisations of the same
        // author's name, but Audnexus brings the same contributor
        // ASIN for both. They should collapse to one `authors` row,
        // with the ASIN populated.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query(
            "INSERT INTO books (book_id, title) VALUES \
                 (1, 'Book A'), \
                 (2, 'Book B')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed books");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, confidence, external_id) \
             VALUES \
               (1, 'author', 'Haruki Murakami', 'audnexus_asin_us', 0.95, 'B0AUTHORX'), \
               (2, 'author', 'Murakami, Haruki', 'audnexus_asin_jp', 0.95, 'B0AUTHORX')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        let stage = IdentityResolveStage::new();
        stage.run(&ctx, BookId(1)).await.expect("run book 1");
        stage.run(&ctx, BookId(2)).await.expect("run book 2");

        // Exactly one row in authors, with the ASIN set; both books
        // point to the same author_id.
        let author_rows: Vec<(i64, String, Option<String>)> =
            sqlx::query_as("SELECT author_id, name, audible_id FROM authors")
                .fetch_all(ctx.library.pool())
                .await
                .expect("read authors");
        assert_eq!(author_rows.len(), 1, "ASIN match collapsed to one row");
        assert_eq!(author_rows[0].2, Some("B0AUTHORX".to_owned()));
        let book_authors: Vec<Option<i64>> =
            sqlx::query_scalar("SELECT author_id FROM books ORDER BY book_id")
                .fetch_all(ctx.library.pool())
                .await
                .expect("read book authors");
        assert_eq!(book_authors[0], book_authors[1], "same author for both");
        assert!(book_authors[0].is_some());
    }

    #[tokio::test]
    async fn name_match_back_fills_audible_id_on_later_run() {
        // First run: tag-read inserted "Brandon Sanderson" without
        // an ASIN. Second run: audnexus-enrich brings the ASIN.
        // The existing row should get its `audible_id` filled in,
        // not a new row created.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Book A')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        // Run 1: tag-read style, no external_id.
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, confidence) \
             VALUES (1, 'author', 'Brandon Sanderson', 'tag_file', 0.7)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed run-1");
        IdentityResolveStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run 1");

        let initial: Option<Option<String>> =
            sqlx::query_scalar("SELECT audible_id FROM authors LIMIT 1")
                .fetch_optional(ctx.library.pool())
                .await
                .expect("audible_id-1");
        assert!(
            initial.is_some() && initial.unwrap().is_none(),
            "no ASIN yet"
        );

        // Run 2: audnexus-enrich brings the ASIN. Append candidate.
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, confidence, external_id) \
             VALUES (1, 'author', 'Brandon Sanderson', 'audnexus_asin_us', 0.95, 'B0SANDXYZ')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed run-2");
        IdentityResolveStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run 2");

        let after: Option<String> =
            sqlx::query_scalar::<_, Option<String>>("SELECT audible_id FROM authors LIMIT 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("audible_id-2");
        assert_eq!(after, Some("B0SANDXYZ".to_owned()), "ASIN back-filled");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM authors")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count");
        assert_eq!(count, 1, "still exactly one row");
    }
}
