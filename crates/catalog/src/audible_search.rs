//! Audible-search ASIN discovery for books with no tag-supplied ASIN.
//!
//! When tag-read finds no `CatalogNumber` atom (so no ASIN candidate
//! exists), `audnexus-enrich` has nothing to look up against.
//! This stage closes that gap: it pulls the best title (+author)
//! candidates from `book_field_provenance`, hits Audible's catalog
//! search, picks the first relevance-ranked result, and writes its
//! ASIN as a low-confidence provenance candidate so `audnexus-enrich`
//! (which runs after this stage) can take it from there.
//!
//! # Confidence model
//!
//! Tag-supplied ASIN: 0.7 (tag-read).
//! Audible-search ASIN: **0.6** (this stage) — lower than tag,
//! because relevance-rank-first-result is a guess. The downstream
//! Audnexus call validates it (if Audnexus 404s the ASIN on every
//! region, the guess was wrong and consensus won't promote anything
//! Audnexus-derived).
//!
//! # When does this skip?
//!
//! - Audible network access disabled in tunables.
//! - The book already has any ASIN candidate (whatever its source).
//!   We don't want a low-confidence Audible guess shadowing a real
//!   tag value.
//! - No title candidate in provenance — we'd be searching for the
//!   empty string.

use async_trait::async_trait;

use ab_core::tunables::NetworkTunables;
use ab_core::{BookId, Error, Field, Result};
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use crate::AudibleClient;

/// Confidence assigned to provenance rows from an Audible-search
/// ASIN guess. Lower than tag-read (0.7) so a confirmed tag value
/// always wins.
pub const AUDIBLE_SEARCH_CONFIDENCE: f64 = 0.6;

/// Provenance source tag for ASINs discovered via Audible search.
pub const PROVENANCE_SOURCE: &str = "audible_search";

/// Stage that discovers an ASIN for ASIN-less books by querying
/// Audible's catalog search.
///
/// Walks `NetworkTunables.audible_region_order` on miss: the
/// home region is tried first, then each fallback (`uk` →
/// `de` → ...). The first region that returns at least one
/// product wins; the matched region is surfaced in the
/// `audible.search.hit` tracing event so operators can see
/// which store actually carries the book.
pub struct AudibleSearchStage {
    client: AudibleClient,
    region_order: Vec<String>,
    allowed: bool,
}

impl AudibleSearchStage {
    /// Build with a pre-configured client + network tunables.
    /// Empty `audible_region_order` falls back to a single
    /// `"us"` entry so the stage always tries at least one
    /// region (matching the `AudnexusEnrichStage` pattern).
    #[must_use]
    pub fn new(client: AudibleClient, network: &NetworkTunables) -> Self {
        let region_order = if network.audible_region_order.is_empty() {
            vec!["us".to_owned()]
        } else {
            network.audible_region_order.clone()
        };
        Self {
            client,
            region_order,
            allowed: network.audible_allowed,
        }
    }
}

/// Typed identifier for this stage. Imported by dependents
/// in their `Stage::requires()` impls.
pub const STAGE_ID: StageId = StageId::new("audible-search");

#[async_trait]
impl Stage for AudibleSearchStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // tag-read writes title+author candidates. Without those we
        // can't even form a search query.
        &[ab_tag_read::STAGE_ID]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        if !self.allowed {
            tracing::debug!(
                book = %book_id,
                "audible.search.disabled_by_tunables"
            );
            return Ok(StageOutcome::Skipped);
        }

        // Defer when an ASIN already exists. The downstream
        // `audnexus-enrich` will use it; running an Audible search
        // would just add a competing low-confidence row.
        if has_asin_candidate(&ctx.library, book_id).await? {
            return Ok(StageOutcome::Skipped);
        }

        let Some(title) = fetch_text_candidate(&ctx.library, book_id, Field::Title).await? else {
            // No title to search on. The book row's filename-derived
            // title is intentionally NOT used here: it tends to be
            // noisy (sample rate, narrator initials, "(Unabridged)"
            // suffixes) and produces poor matches.
            return Ok(StageOutcome::Skipped);
        };
        let author = fetch_text_candidate(&ctx.library, book_id, Field::Author)
            .await?
            .unwrap_or_default();

        // Region walk: try each configured region in order, stop
        // on the first non-empty response. Transport errors in
        // one region log + continue to the next — a single
        // regional outage shouldn't take the whole search down.
        // The order in `NetworkTunables.audible_region_order` is
        // set by the user; default is us → uk → de → fr → ca →
        // au → jp → in → it.
        let mut hit: Option<(String, crate::audible::AudibleProduct)> = None;
        for region in &self.region_order {
            match self.client.search(region, &title, &author).await {
                Ok(products) => {
                    if let Some(first) = products.into_iter().next() {
                        hit = Some((region.clone(), first));
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        book = %book_id,
                        title = %title,
                        region = %region,
                        error = %e,
                        "audible.search.region_failed"
                    );
                    // Continue to the next region — one regional
                    // outage shouldn't abort the walk.
                }
            }
        }

        let Some((region, first)) = hit else {
            tracing::info!(
                book = %book_id,
                title = %title,
                regions_tried = self.region_order.len(),
                "audible.search.no_results"
            );
            return Ok(StageOutcome::Skipped);
        };

        write_asin_candidate(&ctx.library, book_id, &first.asin).await?;
        tracing::info!(
            book = %book_id,
            title = %title,
            asin = %first.asin,
            matched_title = %first.title,
            region = %region,
            "audible.search.hit"
        );
        Ok(StageOutcome::Done)
    }
}

/// True when this book already has an ASIN candidate (any source).
/// Used to skip Audible search when an upstream source already
/// supplied one.
async fn has_asin_candidate(library: &ab_db::LibraryDb, book_id: BookId) -> Result<bool> {
    let id = book_id.0;
    let asin_field = Field::Asin.as_str();
    // The `1 AS "hit!: i64"` annotation pins the column type so
    // sqlx-prepare doesn't fall back to NULL inference on the
    // literal `1` (which it does when the prep DB is empty and
    // the table has been rebuilt by a recent migration —
    // migration 011 was the trigger for fixing this).
    let row = sqlx::query!(
        r#"SELECT 1 AS "hit!: i64" FROM book_field_provenance
           WHERE book_id = ? AND field = ? AND value IS NOT NULL LIMIT 1"#,
        id,
        asin_field,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("audible_search asin check: {e}")))?;
    Ok(row.is_some())
}

/// Highest-confidence non-null candidate for `field`.
async fn fetch_text_candidate(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    field: Field,
) -> Result<Option<String>> {
    let id = book_id.0;
    let field_str = field.as_str();
    let row = sqlx::query!(
        "SELECT value FROM book_field_provenance \
         WHERE book_id = ? AND field = ? AND value IS NOT NULL \
         ORDER BY confidence DESC, recorded_at DESC LIMIT 1",
        id,
        field_str,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("audible_search candidate {field}: {e}")))?;
    Ok(row.and_then(|r| r.value))
}

/// Insert the discovered ASIN as a new provenance row.
async fn write_asin_candidate(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    asin: &str,
) -> Result<()> {
    let id = book_id.0;
    let asin_field = Field::Asin.as_str();
    let stage_str = STAGE_ID.as_str();
    sqlx::query!(
        "INSERT INTO book_field_provenance \
         (book_id, field, value, source, stage, confidence, is_winner) \
         VALUES (?, ?, ?, ?, ?, ?, 0)",
        id,
        asin_field,
        asin,
        PROVENANCE_SOURCE,
        stage_str,
        AUDIBLE_SEARCH_CONFIDENCE,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("audible_search write candidate: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use ab_core::tunables::{DbTunables, HttpClientTunables};
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
            stage_name: "audible-search",
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let client = AudibleClient::new(&HttpClientTunables::default());
        let stage = AudibleSearchStage::new(client, &NetworkTunables::default());
        assert_eq!(stage.name(), "audible-search");
        assert_eq!(stage.requires(), &[ab_tag_read::STAGE_ID]);
    }

    #[tokio::test]
    async fn skips_when_disabled_by_tunables() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        let client = AudibleClient::new(&HttpClientTunables::default());
        let network = NetworkTunables {
            audible_allowed: false,
            ..NetworkTunables::default()
        };
        let stage = AudibleSearchStage::new(client, &network);
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_asin_already_present() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'asin',  'B07XYZ1234', 'tag_file', 'tag-read', 0.7), \
                    (1, 'title', 'Some Book',  'tag_file', 'tag-read', 0.7)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        let client = AudibleClient::new(&HttpClientTunables::default());
        let stage = AudibleSearchStage::new(client, &NetworkTunables::default());
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(
            outcome,
            StageOutcome::Skipped,
            "should defer to existing ASIN source"
        );
    }

    #[tokio::test]
    async fn skips_when_no_title_candidate() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        // No provenance rows.

        let client = AudibleClient::new(&HttpClientTunables::default());
        let stage = AudibleSearchStage::new(client, &NetworkTunables::default());
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Skipped);
    }
}
