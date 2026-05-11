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
//! Case-insensitive matching keeps "Brandon Sanderson" and
//! "brandon sanderson" colliding into one row. ASIN-based matching
//! (when an Audnexus contributor brings an `asin`) is deferred until
//! the `audible_id` column gets populated by a later enrichment.

use std::collections::HashSet;

use async_trait::async_trait;

use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageOutcome};

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

#[async_trait]
impl Stage for IdentityResolveStage {
    fn name(&self) -> &'static str {
        "identity-resolve"
    }

    fn requires(&self) -> &'static [&'static str] {
        // After consensus runs so the direct-column promotions are
        // already in place. Consensus depends on audnexus-enrich,
        // which writes the contributor candidates this stage needs;
        // transitivity covers the ordering.
        &["consensus"]
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
    let Some(name) = pick_winner(tx, book_id, "author").await? else {
        return Ok(false);
    };
    let author_id = find_or_insert_person(tx, "authors", "author_id", &name).await?;
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
    let Some(name) = pick_winner(tx, book_id, "publisher").await? else {
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
    let candidates = fetch_all_distinct(tx, book_id, "narrator").await?;
    if candidates.is_empty() {
        return Ok(0);
    }

    // Resolve every name to an id first so we have a clean target
    // set before touching the junction.
    let mut narrator_ids: Vec<i64> = Vec::with_capacity(candidates.len());
    for name in &candidates {
        let nid = find_or_insert_person(tx, "narrators", "narrator_id", name).await?;
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

/// Highest-confidence non-null candidate for `field`. Returns the
/// trimmed value or `None` if no candidate exists.
async fn pick_winner(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: &str,
) -> Result<Option<String>> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT value FROM book_field_provenance \
         WHERE book_id = ? AND field = ? AND value IS NOT NULL \
         ORDER BY confidence DESC, recorded_at DESC LIMIT 1",
        id,
        field,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity pick {field}: {e}")))?;
    Ok(row
        .and_then(|r| r.value)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty()))
}

/// Every distinct non-null candidate for `field` across all
/// sources, normalised by trim + case-insensitive dedup.
async fn fetch_all_distinct(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: &str,
) -> Result<Vec<String>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        "SELECT value FROM book_field_provenance \
         WHERE book_id = ? AND field = ? AND value IS NOT NULL \
         ORDER BY confidence DESC, recorded_at DESC",
        id,
        field,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity fetch all {field}: {e}")))?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        let Some(v) = r.value else { continue };
        let trimmed = v.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(trimmed.to_owned());
        }
    }
    Ok(out)
}

/// Find an existing author / narrator by case-insensitive name,
/// or insert a new row. Returns the row's id.
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
) -> Result<i64> {
    // Case-insensitive existing-name lookup.
    let select_sql = format!(
        "SELECT {id_column} FROM {table} \
         WHERE lower(name) = lower(?) LIMIT 1"
    );
    let existing: Option<i64> = sqlx::query_scalar(&select_sql)
        .bind(name)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity lookup {table}: {e}")))?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let insert_sql = format!("INSERT INTO {table} (name) VALUES (?) RETURNING {id_column}");
    let new_id: i64 = sqlx::query_scalar(&insert_sql)
        .bind(name)
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
        assert_eq!(stage.requires(), &["consensus"]);
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
}
