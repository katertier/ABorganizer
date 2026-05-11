//! Audnexus enrichment stage.
//!
//! Reads the best ASIN candidate from `book_field_provenance`,
//! looks the book up against Audnexus, and writes the returned
//! metadata back as provenance rows (source = `audnexus_asin`,
//! confidence = 0.95 — higher than tag-file's 0.7 because
//! Audnexus has been hand-curated against Audible's authoritative
//! catalog).
//!
//! # Behaviour
//!
//! - ASIN lookup only — Audible-search ASIN discovery for ASIN-less
//!   books is a follow-up slice.
//! - Region walk: tries every region in
//!   `Tunables::network::audnexus_region_order` in order, stops on
//!   first hit. The matched region is encoded into the provenance
//!   source (`audnexus_asin_<region>`) so the consensus stage can
//!   prefer home-region results.
//! - Provenance written for: title, subtitle, description, language,
//!   publisher, `release_date`, `runtime_length_min`.
//! - `books.asin` is set when the lookup succeeds and the column is
//!   currently NULL. The "winner" semantics for fields with
//!   multiple candidates is the consensus stage's job; this column
//!   exists only for fast ASIN joins on the read path.

use async_trait::async_trait;

use ab_core::tunables::NetworkTunables;
use ab_core::{BookId, Error, Result};
use ab_pipeline::{Stage, StageContext, StageOutcome};

use crate::AudnexusClient;

/// Confidence assigned to provenance rows written from an Audnexus
/// ASIN hit. Picked higher than tag-file (0.7) because Audnexus
/// data tracks Audible's authoritative catalog.
pub const AUDNEXUS_CONFIDENCE: f64 = 0.95;

/// Provenance source tag for Audnexus ASIN lookups.
pub const PROVENANCE_SOURCE: &str = "audnexus_asin";

/// Stage that enriches a book by looking up its tag-supplied ASIN
/// against Audnexus.
pub struct AudnexusEnrichStage {
    client: AudnexusClient,
    region_order: Vec<String>,
    allowed: bool,
}

impl AudnexusEnrichStage {
    /// Build with a pre-configured client + network tunables. The
    /// region order defaults to a single `"us"` entry if the
    /// configured list is empty (so the stage always tries at least
    /// one region).
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
impl Stage for AudnexusEnrichStage {
    fn name(&self) -> &'static str {
        "audnexus-enrich"
    }

    fn requires(&self) -> &'static [&'static str] {
        // tag-read writes the tag-supplied ASIN candidate;
        // audible-search writes a fallback ASIN candidate when no
        // tag value exists. We wait for BOTH so the lookup sees
        // whichever source supplied an ASIN.
        &["tag-read", "audible-search"]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        if !self.allowed {
            tracing::debug!(
                book = %book_id,
                "audnexus.enrich.disabled_by_tunables"
            );
            return Ok(StageOutcome::Skipped);
        }

        let Some(asin) = fetch_asin_candidate(&ctx.library, book_id).await? else {
            // No ASIN in provenance — nothing to enrich against
            // Audnexus. Audible-search ASIN-discovery fallback for
            // ASIN-less books lands in a later slice.
            return Ok(StageOutcome::Skipped);
        };

        // Region walk: try home, then each configured fallback. Stop
        // on the first 200 response. NOT_FOUND in one region just
        // means "try the next" — books with regional Audible-store
        // exclusivity (common for non-US releases) only resolve in
        // their home region. The order in
        // `Tunables::network::audnexus_region_order` is set by the
        // user; default is us → uk → de → fr → ca → au → jp → in → it.
        let mut hit: Option<(String, crate::audnexus::AudnexusBook)> = None;
        for region in &self.region_order {
            match self.client.lookup_book(region, &asin).await {
                Ok(Some(book)) => {
                    hit = Some((region.clone(), book));
                    break;
                }
                Ok(None) => {
                    tracing::debug!(
                        book = %book_id,
                        asin = %asin,
                        region = %region,
                        "audnexus.enrich.region_miss"
                    );
                }
                Err(e) => {
                    // Transport errors (DNS, TLS, timeout) — log + move
                    // on. A single bad region shouldn't fail the whole
                    // walk; the book might still resolve elsewhere.
                    tracing::warn!(
                        book = %book_id,
                        asin = %asin,
                        region = %region,
                        error = %e,
                        "audnexus.enrich.region_error"
                    );
                }
            }
        }

        let Some((region, book)) = hit else {
            tracing::info!(
                book = %book_id,
                asin = %asin,
                regions_tried = self.region_order.len(),
                "audnexus.enrich.all_regions_missed"
            );
            return Ok(StageOutcome::Skipped);
        };

        write_provenance(&ctx.library, book_id, &book, &region).await?;
        promote_asin(&ctx.library, book_id, &book.asin).await?;
        tracing::info!(
            book = %book_id,
            asin = %book.asin,
            region = %region,
            "audnexus.enrich.done"
        );
        Ok(StageOutcome::Done)
    }
}

/// Pick the highest-confidence ASIN candidate from
/// `book_field_provenance`. Most commonly populated by tag-read
/// with the M4B `CatalogNumber` atom. Returns `None` if no
/// candidate exists.
async fn fetch_asin_candidate(
    library: &ab_db::LibraryDb,
    book_id: BookId,
) -> Result<Option<String>> {
    let id = book_id.0;
    // `value` is nullable in the schema (intentional — a provenance
    // row can record "absence of a value"). `.flatten()` collapses
    // the Option<Option<String>> + drops the null-candidate case.
    let row = sqlx::query!(
        "SELECT value FROM book_field_provenance \
         WHERE book_id = ? AND field = 'asin' AND value IS NOT NULL \
         ORDER BY confidence DESC, recorded_at DESC LIMIT 1",
        id,
    )
    .fetch_optional(library.pool())
    .await
    .map_err(|e| Error::Database(format!("audnexus asin lookup: {e}")))?;
    Ok(row.and_then(|r| r.value))
}

/// Write one provenance row per non-empty Audnexus field. The
/// `region` is encoded into the provenance source as
/// `audnexus_asin_<region>` so the consensus stage can prefer
/// home-region matches over fallback-region matches.
async fn write_provenance(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    book: &crate::audnexus::AudnexusBook,
    region: &str,
) -> Result<()> {
    let source = format_source(region);
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("audnexus tx begin: {e}")))?;

    write_scalar_provenance(&mut tx, book_id, &source, book).await?;
    write_contributor_provenance(&mut tx, book_id, &source, book).await?;

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("audnexus tx commit: {e}")))?;
    Ok(())
}

/// Write provenance for the scalar (single-value) fields:
/// title, subtitle, description, language, publisher,
/// `release_date`, `duration_seconds`. None of these carry an
/// `external_id` from Audnexus.
async fn write_scalar_provenance(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    source: &str,
    book: &crate::audnexus::AudnexusBook,
) -> Result<()> {
    insert_scalar(tx, book_id, "title", &book.title, source).await?;
    if let Some(v) = book.subtitle.as_deref() {
        insert_scalar(tx, book_id, "subtitle", v, source).await?;
    }
    if let Some(v) = book.description.as_deref() {
        insert_scalar(tx, book_id, "description", v, source).await?;
    }
    if let Some(v) = book.language.as_deref() {
        insert_scalar(tx, book_id, "language", v, source).await?;
    }
    if let Some(v) = book.publisher_name.as_deref() {
        insert_scalar(tx, book_id, "publisher", v, source).await?;
    }
    if let Some(v) = book.release_date.as_deref() {
        insert_scalar(tx, book_id, "release_date", v, source).await?;
    }
    if let Some(minutes) = book.runtime_length_min {
        // Store as decimal-seconds string so the provenance row's
        // TEXT value column can hold it alongside the other strings.
        // The consensus stage will parse it back. Picked text-over-
        // separate-column to avoid widening the provenance schema
        // for one numeric field.
        let secs = i64::from(minutes).saturating_mul(60);
        let secs_str = secs.to_string();
        insert_scalar(tx, book_id, "duration_seconds", &secs_str, source).await?;
    }
    Ok(())
}

/// Convenience wrapper for scalar-source-no-external_id rows;
/// keeps the call sites inside `write_scalar_provenance` short
/// and uniform without a closure (the closure tripped a
/// borrow-checker lifetime fight; this fn-sig variant doesn't).
async fn insert_scalar(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: &'static str,
    value: &str,
    source: &str,
) -> Result<()> {
    insert_row(
        tx,
        ProvenanceRow {
            book_id,
            field,
            value,
            source,
            external_id: None,
        },
    )
    .await
}

/// Write one provenance row per Audnexus contributor (author or
/// narrator). The contributor's Audnexus ASIN (when present) goes
/// into the `external_id` column so identity-resolve can match
/// against `authors.audible_id` / `narrators.audible_id`.
async fn write_contributor_provenance(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    source: &str,
    book: &crate::audnexus::AudnexusBook,
) -> Result<()> {
    for author in &book.authors {
        let name = author.name.trim();
        if name.is_empty() {
            continue;
        }
        insert_row(
            tx,
            ProvenanceRow {
                book_id,
                field: "author",
                value: name,
                source,
                external_id: author.asin.as_deref(),
            },
        )
        .await?;
    }
    for narrator in &book.narrators {
        let name = narrator.name.trim();
        if name.is_empty() {
            continue;
        }
        insert_row(
            tx,
            ProvenanceRow {
                book_id,
                field: "narrator",
                value: name,
                source,
                external_id: narrator.asin.as_deref(),
            },
        )
        .await?;
    }
    Ok(())
}

/// Compose the provenance source tag for a successful Audnexus
/// lookup in a given region.
fn format_source(region: &str) -> String {
    // Region is restricted to lowercase ASCII letters in our
    // tunables; no escaping needed. Worst case the format is
    // tolerant of unexpected chars (we'd just store an odd source
    // tag).
    format!("{PROVENANCE_SOURCE}_{region}")
}

/// Bundle of arguments to `insert_row`; promoted from 6 positional
/// params to a small struct so the function stays under the
/// `clippy::too_many_arguments` cap (5). Keeps each call site
/// readable without macro tricks.
struct ProvenanceRow<'a> {
    book_id: BookId,
    field: &'a str,
    value: &'a str,
    source: &'a str,
    external_id: Option<&'a str>,
}

async fn insert_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: ProvenanceRow<'_>,
) -> Result<()> {
    let id = row.book_id.0;
    sqlx::query!(
        "INSERT INTO book_field_provenance \
         (book_id, field, value, source, confidence, is_winner, external_id) \
         VALUES (?, ?, ?, ?, ?, 0, ?)",
        id,
        row.field,
        row.value,
        row.source,
        AUDNEXUS_CONFIDENCE,
        row.external_id,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("audnexus provenance {}: {e}", row.field)))?;
    Ok(())
}

/// Set `books.asin` to the looked-up ASIN if the column is currently
/// NULL. Subsequent enrichers (re-runs, multi-source consensus) may
/// overwrite via a different code path; this just primes the read
/// index.
async fn promote_asin(library: &ab_db::LibraryDb, book_id: BookId, asin: &str) -> Result<()> {
    let id = book_id.0;
    sqlx::query!(
        "UPDATE books SET asin = ? WHERE book_id = ? AND asin IS NULL",
        asin,
        id,
    )
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("audnexus promote asin: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use ab_core::tunables::{DbTunables, HttpClientTunables};
    use tempfile::TempDir;

    use super::*;
    use crate::AudnexusClient;

    async fn fresh_db(dir: &std::path::Path) -> ab_db::LibraryDb {
        let path = dir.join("library.db");
        ab_db::LibraryDb::open(&path, &DbTunables::default())
            .await
            .expect("open db")
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let client = AudnexusClient::new(&HttpClientTunables::default());
        let stage = AudnexusEnrichStage::new(client, &NetworkTunables::default());
        assert_eq!(stage.name(), "audnexus-enrich");
        assert_eq!(stage.requires(), &["tag-read", "audible-search"]);
    }

    #[test]
    fn region_order_preserved_from_tunables() {
        let client = AudnexusClient::new(&HttpClientTunables::default());
        let network = NetworkTunables {
            audnexus_region_order: vec!["de".into(), "uk".into(), "us".into()],
            ..NetworkTunables::default()
        };
        let stage = AudnexusEnrichStage::new(client, &network);
        assert_eq!(stage.region_order, vec!["de", "uk", "us"]);
    }

    #[test]
    fn region_order_falls_back_when_empty() {
        let client = AudnexusClient::new(&HttpClientTunables::default());
        let network = NetworkTunables {
            audnexus_region_order: Vec::new(),
            ..NetworkTunables::default()
        };
        let stage = AudnexusEnrichStage::new(client, &network);
        assert_eq!(stage.region_order, vec!["us"]);
    }

    #[test]
    fn provenance_source_encodes_region() {
        assert_eq!(format_source("us"), "audnexus_asin_us");
        assert_eq!(format_source("de"), "audnexus_asin_de");
    }

    #[tokio::test]
    async fn skips_when_disabled_by_tunables() {
        let tmp = TempDir::new().expect("tmpdir");
        let db = fresh_db(tmp.path()).await;
        let client = AudnexusClient::new(&HttpClientTunables::default());
        let network = NetworkTunables {
            audnexus_allowed: false,
            ..NetworkTunables::default()
        };
        let stage = AudnexusEnrichStage::new(client, &network);
        let ctx = StageContext {
            library: db,
            ephemeral: ab_db::EphemeralDb::open(
                &tmp.path().join("ephemeral.db"),
                &DbTunables::default(),
            )
            .await
            .expect("open ephemeral"),
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: "audnexus-enrich",
        };
        let outcome = stage
            .run(&ctx, BookId(1))
            .await
            .expect("run should not fail when disabled");
        assert_eq!(outcome, StageOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_no_asin_in_provenance() {
        let tmp = TempDir::new().expect("tmpdir");
        let db = fresh_db(tmp.path()).await;
        // Insert a book but no provenance.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'No-ASIN Book')")
            .execute(db.pool())
            .await
            .expect("insert book");

        let client = AudnexusClient::new(&HttpClientTunables::default());
        let stage = AudnexusEnrichStage::new(client, &NetworkTunables::default());
        let ctx = StageContext {
            library: db,
            ephemeral: ab_db::EphemeralDb::open(
                &tmp.path().join("ephemeral.db"),
                &DbTunables::default(),
            )
            .await
            .expect("open ephemeral"),
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name: "audnexus-enrich",
        };
        let outcome = stage
            .run(&ctx, BookId(1))
            .await
            .expect("run should succeed with skip when no ASIN");
        assert_eq!(outcome, StageOutcome::Skipped);
    }
}
