//! Audnexus enrichment stage.
//!
//! Reads the best ASIN candidate from `book_field_provenance`,
//! looks the book up against Audnexus, and writes the returned
//! metadata back as provenance rows (source = `audnexus_asin`,
//! confidence = 0.95 — higher than tag-file's 0.7 because
//! Audnexus has been hand-curated against Audible's authoritative
//! catalog).
//!
//! # Slice 2A scope
//!
//! - ASIN lookup only (no Audible-search fallback — slice 2B).
//! - Single home region from `Tunables::network::audnexus_region_order[0]`
//!   (region walk lives in slice 2C alongside Audible fallback).
//! - Provenance for: title, subtitle, description, language,
//!   publisher, `release_date`, `runtime_length_min`.
//! - `books.asin` column updated when the lookup succeeds and the
//!   column is currently NULL (the "winner" gets promoted by a
//!   later consensus stage; this column is just for fast ASIN
//!   joins on the read path).

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
    home_region: String,
    allowed: bool,
}

impl AudnexusEnrichStage {
    /// Build with a pre-configured client + network tunables. The
    /// home region defaults to `"us"` if the configured region list
    /// is empty.
    #[must_use]
    pub fn new(client: AudnexusClient, network: &NetworkTunables) -> Self {
        let home_region = network
            .audnexus_region_order
            .first()
            .cloned()
            .unwrap_or_else(|| "us".to_owned());
        Self {
            client,
            home_region,
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
        // tag-read writes the ASIN candidate. Without it we have no
        // key to look up against Audnexus.
        &["tag-read"]
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
            // Audnexus. Slice 2B adds an Audible-search fallback for
            // ASIN-less books.
            return Ok(StageOutcome::Skipped);
        };

        let book = match self.client.lookup_book(&self.home_region, &asin).await {
            Ok(Some(book)) => book,
            Ok(None) => {
                tracing::info!(
                    book = %book_id,
                    asin = %asin,
                    region = %self.home_region,
                    "audnexus.enrich.miss"
                );
                return Ok(StageOutcome::Skipped);
            }
            Err(e) => {
                tracing::warn!(
                    book = %book_id,
                    asin = %asin,
                    region = %self.home_region,
                    error = %e,
                    "audnexus.enrich.lookup_failed"
                );
                return Err(e);
            }
        };

        write_provenance(&ctx.library, book_id, &book).await?;
        promote_asin(&ctx.library, book_id, &book.asin).await?;
        tracing::info!(
            book = %book_id,
            asin = %book.asin,
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

/// Write one provenance row per non-empty Audnexus field.
async fn write_provenance(
    library: &ab_db::LibraryDb,
    book_id: BookId,
    book: &crate::audnexus::AudnexusBook,
) -> Result<()> {
    let mut tx = library
        .pool()
        .begin()
        .await
        .map_err(|e| Error::Database(format!("audnexus tx begin: {e}")))?;

    insert_row(&mut tx, book_id, "title", &book.title).await?;
    if let Some(v) = book.subtitle.as_deref() {
        insert_row(&mut tx, book_id, "subtitle", v).await?;
    }
    if let Some(v) = book.description.as_deref() {
        insert_row(&mut tx, book_id, "description", v).await?;
    }
    if let Some(v) = book.language.as_deref() {
        insert_row(&mut tx, book_id, "language", v).await?;
    }
    if let Some(v) = book.publisher_name.as_deref() {
        insert_row(&mut tx, book_id, "publisher", v).await?;
    }
    if let Some(v) = book.release_date.as_deref() {
        insert_row(&mut tx, book_id, "release_date", v).await?;
    }
    if let Some(minutes) = book.runtime_length_min {
        // Store as decimal-seconds string so the provenance row's
        // TEXT value column can hold it alongside the other strings.
        // The consensus stage will parse it back. Picked text-over-
        // separate-column to avoid widening the provenance schema
        // for one numeric field.
        let secs = i64::from(minutes).saturating_mul(60);
        insert_row(&mut tx, book_id, "duration_seconds", &secs.to_string()).await?;
    }

    tx.commit()
        .await
        .map_err(|e| Error::Database(format!("audnexus tx commit: {e}")))?;
    Ok(())
}

async fn insert_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    field: &str,
    value: &str,
) -> Result<()> {
    let id = book_id.0;
    sqlx::query!(
        "INSERT INTO book_field_provenance \
         (book_id, field, value, source, confidence, is_winner) \
         VALUES (?, ?, ?, ?, ?, 0)",
        id,
        field,
        value,
        PROVENANCE_SOURCE,
        AUDNEXUS_CONFIDENCE,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("audnexus provenance {field}: {e}")))?;
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
        assert_eq!(stage.requires(), &["tag-read"]);
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
