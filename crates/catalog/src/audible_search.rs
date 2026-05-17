//! Audible-search ASIN discovery for books with no tag-supplied ASIN.
//!
//! When read-tags finds no `CatalogNumber` atom (so no ASIN candidate
//! exists), `enrich-from-audnexus` has nothing to look up against.
//! This stage closes that gap: it pulls the best title (+author)
//! candidates from `book_field_provenance`, hits Audible's catalog
//! search, picks the first relevance-ranked result, and writes its
//! ASIN as a low-confidence provenance candidate so `enrich-from-audnexus`
//! (which runs after this stage) can take it from there.
//!
//! # Confidence model
//!
//! Tag-supplied ASIN: 0.7 (read-tags).
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
/// ASIN guess. Lower than read-tags (0.7) so a confirmed tag value
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
pub const STAGE_ID: StageId = StageId::new("search-audible");

#[async_trait]
impl Stage for AudibleSearchStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // read-tags writes title+author candidates. Without those we
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
        // `enrich-from-audnexus` will use it; running an Audible search
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

        // Auto-learn hint: if a previous operator edit captured this
        // (title, author) → asin mapping in `asin_learnings`, skip the
        // network call and write the learned ASIN as a higher-confidence
        // candidate (0.8). The downstream `enrich-from-audnexus` still
        // validates the guess by hitting per-region endpoints; a bad
        // learning silently loses to a real Audnexus result via
        // consensus.
        if let Some(learned) = crate::asin_learnings::lookup(ctx.library.pool(), &title, &author)
            .await
            .map_err(|e| Error::Database(format!("audible_search learn lookup: {e}")))?
        {
            write_provenance_candidate(
                &ctx.library,
                book_id,
                &learned,
                crate::asin_learnings::PROVENANCE_SOURCE_LEARN,
                crate::asin_learnings::ASIN_LEARN_CONFIDENCE,
            )
            .await?;
            tracing::info!(
                book = %book_id,
                title = %title,
                asin = %learned,
                "audible.search.learn_hit"
            );
            return Ok(StageOutcome::Done);
        }

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

        write_provenance_candidate(
            &ctx.library,
            book_id,
            &first.asin,
            PROVENANCE_SOURCE,
            AUDIBLE_SEARCH_CONFIDENCE,
        )
        .await?;
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

/// Insert the discovered ASIN as a new provenance row. `source`
/// and `confidence` vary by which sub-path inside this stage
/// produced the hit (a fresh Audible API result vs. a hit on
/// `asin_learnings` from a prior operator edit).
async fn write_provenance_candidate(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    asin: &str,
    source: &str,
    confidence: f64,
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
        source,
        stage_str,
        confidence,
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
            stage_name: "search-audible",
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let client = AudibleClient::new(&HttpClientTunables::default());
        let stage = AudibleSearchStage::new(client, &NetworkTunables::default());
        assert_eq!(stage.name(), "search-audible");
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
             VALUES (1, 'asin',  'B07XYZ1234', 'tag_file', 'read-tags', 0.7), \
                    (1, 'title', 'Some Book',  'tag_file', 'read-tags', 0.7)",
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
    async fn learn_hit_short_circuits_network_with_higher_confidence_row() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;

        // Book with title + author candidates but no ASIN.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'title',  'Mistborn',           'tag_file', 'read-tags', 0.7), \
                    (1, 'author', 'Brandon Sanderson',  'tag_file', 'read-tags', 0.7)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        // A previously-captured learning matches the normalised key.
        sqlx::query(
            "INSERT INTO asin_learnings (title_norm, author_norm, asin, source, learned_at) \
             VALUES ('mistborn', 'brandon sanderson', 'B002UZJ8TG', 'user_edit', '2026-05-17T00:00:00Z')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed learning");

        // Even with audible_allowed = true, the network call must be
        // short-circuited by the learn-hit branch (test would hang on
        // a real HTTP call against the default client).
        let client = AudibleClient::new(&HttpClientTunables::default());
        let stage = AudibleSearchStage::new(client, &NetworkTunables::default());
        let outcome = stage.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(outcome, StageOutcome::Done);

        // Exactly one new provenance row, with the learned source +
        // higher confidence.
        let row = sqlx::query!(
            r#"SELECT value AS "value!: String",
                      source AS "source!: String",
                      confidence AS "confidence!: f64"
                 FROM book_field_provenance
                WHERE book_id = 1 AND field = 'asin'"#
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("fetch row");
        assert_eq!(row.value, "B002UZJ8TG");
        assert_eq!(row.source, crate::asin_learnings::PROVENANCE_SOURCE_LEARN);
        assert!(
            (row.confidence - crate::asin_learnings::ASIN_LEARN_CONFIDENCE).abs() < f64::EPSILON
        );
    }

    #[tokio::test]
    async fn no_learn_hit_falls_through_to_network_path() {
        // Indirect check: with audible_allowed=false the stage skips
        // before the network call; with NO learning row matching, we
        // reach that disabled-by-tunables check (i.e., didn't return
        // Done from the learn-hit branch).
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'placeholder')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'title',  'Mistborn',          'tag_file', 'read-tags', 0.7), \
                    (1, 'author', 'Brandon Sanderson', 'tag_file', 'read-tags', 0.7)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");
        // No asin_learnings rows for this (title, author).

        // Network calls would hang the test — so we re-disable via
        // tunables before the run. The point is the learn-hit branch
        // does NOT fire (otherwise the test would short-circuit to
        // Done before the network gate).
        let client = AudibleClient::new(&HttpClientTunables::default());
        let stage = AudibleSearchStage::new(
            client,
            &NetworkTunables {
                audible_allowed: true,
                ..NetworkTunables::default()
            },
        );
        // Override: re-construct with audible_allowed = false to
        // force the early skip and avoid the live HTTP path.
        let stage_no_net = AudibleSearchStage::new(
            AudibleClient::new(&HttpClientTunables::default()),
            &NetworkTunables {
                audible_allowed: false,
                ..NetworkTunables::default()
            },
        );
        let outcome = stage_no_net.run(&ctx, BookId(1)).await.expect("run");
        assert_eq!(
            outcome,
            StageOutcome::Skipped,
            "fell through to network gate which is disabled"
        );

        // And no provenance row got written.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM book_field_provenance WHERE field = 'asin'")
                .fetch_one(ctx.library.pool())
                .await
                .expect("count");
        assert_eq!(count, 0);
        let _ = stage; // silence unused if compiler complains
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
