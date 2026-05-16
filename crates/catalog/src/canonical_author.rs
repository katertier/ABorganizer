//! Canonical author enrichment stage.
//!
//! After [`crate::identity::IdentityResolveStage`] runs, each book's
//! `authors` row carries (when known) the author's Audible ASIN in
//! `audible_id`. This stage walks the configured Audnexus region
//! order and populates `authors.bio` + `authors.image_url` from
//! `/authors/{ASIN}` for any author still missing those fields.
//!
//! ## Why a per-book stage
//!
//! Pipeline-wise, every other enrichment is per-book; book-stage
//! ordering and the retry / reset semantics are battle-tested.
//! A per-book invocation is also naturally idempotent: re-running
//! after the author is already enriched is a single SQL `SELECT`
//! that returns "bio is not NULL" → `Skipped`. The downside (we
//! may pay a redundant lookup if 50 books credit the same author
//! and only the first call actually fills the row) is dominated
//! by Audnexus latency, not work this code does — and gating on
//! `bio IS NULL` before the network call avoids any wasted
//! Audnexus call.
//!
//! ## Behaviour
//!
//! * Skip when `books.author_id` is `NULL` (identity-resolve
//!   hasn't matched the book to a canonical author yet).
//! * Skip when `authors.audible_id` is `NULL` (no ASIN to lookup
//!   against — author came from a tag-only path or fuzzy match
//!   that didn't produce an ASIN).
//! * Skip when `authors.bio` is non-NULL and non-empty (already
//!   enriched by a prior run, including a prior Audnexus call
//!   that returned an empty description — we don't re-try).
//! * Walk regions in tunables order; stop on first hit. The
//!   region that responded is logged for diagnostics.
//! * Update is one `UPDATE authors` per book; `updated_at` is
//!   bumped via `strftime('%s','now')`.
//!
//! ## What it does NOT touch
//!
//! * The book's `authors[]` array as returned by Audnexus's
//!   `/books/{ASIN}` endpoint — `enrich-from-audnexus` already
//!   writes those contributor names + ASINs to
//!   `book_field_provenance`.
//! * The `genres` array on Audnexus's author response — author-
//!   level genres don't yet have a canonical home in the schema;
//!   future slice.
//! * Narrators — same story, plus narrators are 1..N per book
//!   so a per-book lookup pattern needs a different shape.

use async_trait::async_trait;

use ab_core::tunables::NetworkTunables;
use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use crate::AudnexusClient;

/// Typed identifier for this stage. Renamed-clean alongside the
/// rest of the pipeline (no bare-noun suffix; verb-led).
pub const STAGE_ID: StageId = StageId::new("enrich-canonical-author");

/// Stage that enriches the book's primary author via Audnexus
/// `/authors/{ASIN}`.
pub struct CanonicalAuthorEnrichStage {
    client: AudnexusClient,
    region_order: Vec<String>,
    allowed: bool,
}

impl CanonicalAuthorEnrichStage {
    /// Build with a pre-configured client + network tunables.
    /// Defaults to a single `"us"` region when the configured
    /// list is empty — same posture as `AudnexusEnrichStage`.
    #[must_use]
    pub fn new(client: AudnexusClient, network: &NetworkTunables) -> Self {
        let region_order = if network.audnexus_region_order.is_empty() {
            vec!["us".to_owned()]
        } else {
            network.audnexus_region_order.clone()
        };
        Self {
            client,
            region_order,
            allowed: network.audnexus_allowed,
        }
    }
}

#[async_trait]
impl Stage for CanonicalAuthorEnrichStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // Need identity-resolve to have populated
        // `books.author_id` + `authors.audible_id`. The Audnexus
        // *book* enrich is a prerequisite of identity-resolve in
        // practice (it's what supplies the author ASIN), so
        // listing identity-resolve transitively covers it.
        &[crate::identity::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        if !self.allowed {
            tracing::debug!(
                book = %book_id,
                "canonical_author.disabled_by_tunables"
            );
            return Ok(StageOutcome::Skipped);
        }

        let Some(target) = fetch_author_target(&ctx.library, book_id).await? else {
            // No author_id OR no audible_id OR already enriched
            // (bio non-NULL). All three roll up to "nothing to do".
            return Ok(StageOutcome::Skipped);
        };

        let mut hit: Option<(String, crate::audnexus::AudnexusAuthor)> = None;
        for region in &self.region_order {
            match self.client.lookup_author(region, &target.audible_id).await {
                Ok(Some(author)) => {
                    hit = Some((region.clone(), author));
                    break;
                }
                Ok(None) => {
                    tracing::debug!(
                        book = %book_id,
                        author_id = %target.author_id,
                        asin = %target.audible_id,
                        region = %region,
                        "canonical_author.region_miss"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        book = %book_id,
                        author_id = %target.author_id,
                        asin = %target.audible_id,
                        region = %region,
                        error = %e,
                        "canonical_author.region_error"
                    );
                }
            }
        }

        let Some((region, author)) = hit else {
            tracing::info!(
                book = %book_id,
                author_id = %target.author_id,
                asin = %target.audible_id,
                regions_tried = self.region_order.len(),
                "canonical_author.all_regions_missed"
            );
            return Ok(StageOutcome::Skipped);
        };

        let bio = author.description.as_deref().and_then(trimmed);
        let image = author.image.as_deref().and_then(trimmed);
        update_author(&ctx.library, target.author_id, bio, image).await?;
        tracing::info!(
            book = %book_id,
            author_id = %target.author_id,
            asin = %target.audible_id,
            region = %region,
            "canonical_author.enriched"
        );
        Ok(StageOutcome::Done)
    }
}

/// Author-side data we need from the DB. Returned as a struct so
/// the call site doesn't juggle three nullable columns.
struct AuthorTarget {
    author_id: i64,
    audible_id: String,
}

/// Fetch the book's author target row. Returns `None` when the
/// book has no author yet, or the author has no ASIN, or the
/// author has already been enriched (bio non-NULL + non-empty).
async fn fetch_author_target(
    library: &ab_db::LibraryDb,
    book_id: BookId,
) -> Result<Option<AuthorTarget>> {
    let id = book_id.0;
    let row = sqlx::query!(
        "SELECT a.author_id AS \"author_id!: i64\", \
                a.audible_id AS \"audible_id?: String\", \
                a.bio AS \"bio?: String\" \
         FROM books b \
         JOIN authors a ON a.author_id = b.author_id \
         WHERE b.book_id = ?",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("canonical_author author lookup: {e}")))?;

    let Some(r) = row else {
        return Ok(None);
    };
    let Some(audible_id) = r.audible_id.filter(|v| !v.trim().is_empty()) else {
        return Ok(None);
    };
    if r.bio.as_ref().is_some_and(|v| !v.trim().is_empty()) {
        return Ok(None);
    }
    Ok(Some(AuthorTarget {
        author_id: r.author_id,
        audible_id,
    }))
}

/// Update `authors.bio` + `authors.image_url` for one row. Both
/// parameters land as-is (`NULL` when the Audnexus response had
/// nothing meaningful). The `updated_at` column is bumped.
async fn update_author(
    library: &ab_db::LibraryDb,
    author_id: i64,
    bio: Option<&str>,
    image_url: Option<&str>,
) -> Result<()> {
    sqlx::query!(
        "UPDATE authors \
         SET bio = ?, \
             image_url = ?, \
             updated_at = strftime('%s','now') \
         WHERE author_id = ?",
        bio,
        image_url,
        author_id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("canonical_author update: {e}")))?;
    Ok(())
}

/// `Some(trim) when non-empty else None`. Audnexus returns
/// "" for missing rather than `null`, so we normalise here.
fn trimmed(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t) }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;

    async fn fresh_library() -> ab_db::LibraryDb {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("library.db");
        let library = ab_db::LibraryDb::open(&db_path, &DbTunables::default())
            .await
            .expect("open library");
        // Tempdir kept alive by the leaked Box — tests run for ~ms.
        Box::leak(Box::new(tmp));
        library
    }

    async fn seed_book_with_author(
        library: &ab_db::LibraryDb,
        audible_id: Option<&str>,
        bio: Option<&str>,
    ) -> (BookId, i64) {
        // Use explicit author_id = 1 so last_insert_rowid() across
        // pool connections isn't load-bearing.
        sqlx::query("INSERT INTO authors (author_id, name, audible_id, bio) VALUES (1, ?, ?, ?)")
            .bind("Some Author")
            .bind(audible_id)
            .bind(bio)
            .execute(library.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO books (book_id, title, author_id) VALUES (1, 'T', 1)")
            .execute(library.pool())
            .await
            .unwrap();
        (BookId(1), 1)
    }

    #[tokio::test]
    async fn fetch_target_skips_when_author_id_null() {
        let library = fresh_library().await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'T')")
            .execute(library.pool())
            .await
            .unwrap();
        let target = fetch_author_target(&library, BookId(1)).await.unwrap();
        assert!(target.is_none());
    }

    #[tokio::test]
    async fn fetch_target_skips_when_audible_id_null() {
        let library = fresh_library().await;
        seed_book_with_author(&library, None, None).await;
        let target = fetch_author_target(&library, BookId(1)).await.unwrap();
        assert!(target.is_none());
    }

    #[tokio::test]
    async fn fetch_target_skips_when_audible_id_blank() {
        let library = fresh_library().await;
        seed_book_with_author(&library, Some("   "), None).await;
        let target = fetch_author_target(&library, BookId(1)).await.unwrap();
        assert!(target.is_none());
    }

    #[tokio::test]
    async fn fetch_target_skips_when_already_enriched() {
        let library = fresh_library().await;
        seed_book_with_author(&library, Some("B000ABCDEF"), Some("Pre-existing bio")).await;
        let target = fetch_author_target(&library, BookId(1)).await.unwrap();
        assert!(target.is_none(), "skip when bio already set");
    }

    #[tokio::test]
    async fn fetch_target_returns_target_when_eligible() {
        let library = fresh_library().await;
        let (book_id, author_id) = seed_book_with_author(&library, Some("B0034Q40L2"), None).await;
        let target = fetch_author_target(&library, book_id)
            .await
            .unwrap()
            .expect("eligible");
        assert_eq!(target.author_id, author_id);
        assert_eq!(target.audible_id, "B0034Q40L2");
    }

    #[tokio::test]
    async fn fetch_target_skips_when_bio_is_empty_string() {
        let library = fresh_library().await;
        seed_book_with_author(&library, Some("B0034Q40L2"), Some("   ")).await;
        // Whitespace-only bio is "still empty" → eligible.
        let target = fetch_author_target(&library, BookId(1))
            .await
            .unwrap()
            .expect("eligible (whitespace bio)");
        assert_eq!(target.audible_id, "B0034Q40L2");
    }

    #[tokio::test]
    async fn update_author_sets_bio_and_image() {
        let library = fresh_library().await;
        let (_book_id, author_id) = seed_book_with_author(&library, Some("B0034Q40L2"), None).await;
        update_author(
            &library,
            author_id,
            Some("Acclaimed novelist..."),
            Some("https://m.media-amazon.com/images/I/X.jpg"),
        )
        .await
        .unwrap();
        let row = sqlx::query!(
            "SELECT bio AS \"bio?: String\", image_url AS \"image_url?: String\" \
             FROM authors WHERE author_id = ?",
            author_id,
        )
        .fetch_one(library.pool())
        .await
        .unwrap();
        assert_eq!(row.bio.as_deref(), Some("Acclaimed novelist..."));
        assert_eq!(
            row.image_url.as_deref(),
            Some("https://m.media-amazon.com/images/I/X.jpg"),
        );
    }

    #[test]
    fn trimmed_helper() {
        assert_eq!(trimmed(""), None);
        assert_eq!(trimmed("   "), None);
        assert_eq!(trimmed("  hi  "), Some("hi"));
    }
}
