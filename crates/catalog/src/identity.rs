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
//!    match. Wins for sources with no canonical id (read-tags).
//!
//! When inserting a new row that has an ASIN, the ASIN is stored
//! in `audible_id` so subsequent runs benefit from the unique
//! partial index. When a name-match wins on a row that's missing
//! `audible_id` and the current candidate has one, the existing
//! row is updated to fill it in.
//!
//! # Alias junction (slice H.3.2, ADR-0026)
//!
//! Every observed spelling is registered in the appropriate
//! `*_aliases` junction (`author_aliases`, `narrator_aliases`,
//! `series_aliases`). The first row inserted gets
//! `source='canonical' is_prime=1`; subsequent spellings get
//! `source` from the candidate's origin and `is_prime=0`. The
//! partial unique index on the junction enforces "at most one
//! prime per parent." Manual exaltation (H.3.4) flips
//! `is_prime` between rows.
//!
//! Match-time lookup uses the junction's `COLLATE NOCASE` index
//! so the same alias attached to multiple parents (two David
//! Mitchells — see ADR-0026) is observable as a multi-row hit.
//!
//! # Ambiguity resolution (slice H.3.5, ADR-0026)
//!
//! When multiple parents share an alias, [`corroborate_author`] /
//! [`corroborate_narrator`] / [`corroborate_series`] score each
//! candidate against the book's already-known signals (narrator
//! overlap, publisher overlap, series-author overlap).
//! [`CORROBORATION_MARGIN`] sets the confidence threshold —
//! winners with a margin below it land in
//! `*_disambiguation_pending` for operator review via the
//! `aborg names resolve` surface (H.3.6).
//!
//! Pending state means "don't write the FK; surface a row." The
//! book's `author_id` / etc. stays NULL until the operator
//! resolves the pending row.

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
pub const STAGE_ID: StageId = StageId::new("resolve-identity");

#[async_trait]
impl Stage for IdentityResolveStage {
    fn name(&self) -> &'static str {
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        // After consensus runs so the direct-column promotions are
        // already in place. Consensus depends on enrich-from-audnexus,
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
        let series_count = resolve_series(&mut tx, book_id).await?;
        if series_count > 0 {
            updates += 1;
        }

        tx.commit()
            .await
            .map_err(|e| Error::Database(format!("identity tx commit: {e}")))?;

        tracing::debug!(
            book = %book_id,
            updates,
            narrator_count,
            series_count,
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
    let resolved = find_or_insert_person(
        tx,
        "authors",
        "author_id",
        &candidate.name,
        candidate.external_id.as_deref(),
        book_id,
    )
    .await?;
    let Some(author_id) = resolved else {
        // Disambiguation pending — leave `books.author_id` NULL,
        // pending row was written by `find_or_insert_person`.
        // Operator resolves via `aborg names resolve`.
        tracing::info!(book = %book_id, "identity.author.deferred_pending");
        return Ok(false);
    };
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

/// One distinct series for a book, after collapsing duplicate
/// `(book_id, lower(series_name))` rows across sources. Built
/// inside [`resolve_series`] from highest-confidence-first
/// candidates so the merge rule "fill blanks from lower-confidence
/// rows" lands in a single pass.
struct SeriesGroup {
    display_name: String,
    asin: Option<String>,
    position: Option<f64>,
    is_primary: bool,
}

/// Resolve series candidates from `book_series_candidate` (slice
/// C5.6's fallback chain: Audnexus → tag → future filename) into
/// the `series` table + `book_series` junction.
///
/// Match precedence within a series candidate group:
/// 1. `series_asin` (Audnexus seriesPrimary / seriesSecondary id) →
///    `series.audible_id` lookup. Wins when present and the
///    series row was previously seeded by an Audnexus call.
/// 2. Case-insensitive `series_name` match → `series.name`. Wins
///    for tag-only candidates and as fallback when ASIN lookup
///    misses.
///
/// Within a `(book_id, lower(series_name))` group:
/// - Highest-confidence row's ASIN seeds the `series.audible_id`
///   on first insert (back-fill on later runs is handled by
///   `find_or_insert_series` when a higher-confidence row arrives
///   carrying the ASIN).
/// - Highest-confidence row's `position` becomes the
///   `book_series.position` for the book.
/// - `is_primary` is OR'd across the group — if any source called
///   this series primary for the book, it stays primary.
///
/// Clear-then-add for `book_series`: re-runs converge on the
/// current candidate set (matching `resolve_narrators` shape).
///
/// Returns the count of series rows linked.
async fn resolve_series(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
) -> Result<usize> {
    let id = book_id.0;
    // Read all series candidates for this book, highest confidence
    // first. Group key is lower(series_name) — same series name
    // across multiple sources collapses to one group.
    let rows = sqlx::query!(
        r#"SELECT
              series_name  AS "series_name!",
              series_asin,
              position,
              is_primary   AS "is_primary!: i64",
              confidence   AS "confidence!: f64"
           FROM book_series_candidate
           WHERE book_id = ?
           ORDER BY confidence DESC, recorded_at DESC"#,
        id,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity fetch series candidates: {e}")))?;

    if rows.is_empty() {
        return Ok(0);
    }

    // Group by lower(series_name) preserving the first (highest-
    // confidence) row's ASIN / position. is_primary OR'd across
    // the group.
    let mut groups: Vec<SeriesGroup> = Vec::new();
    let mut keys: HashSet<String> = HashSet::new();
    for r in rows {
        let trimmed = r.series_name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        let primary = r.is_primary != 0;
        if let Some(existing) = groups
            .iter_mut()
            .find(|g| g.display_name.eq_ignore_ascii_case(trimmed))
        {
            // Higher-confidence row was already seen (rows arrive
            // confidence-DESC). Fill in pieces it lacks.
            if existing.asin.is_none() {
                if let Some(a) = r.series_asin.as_deref() {
                    if !a.is_empty() {
                        existing.asin = Some(a.to_owned());
                    }
                }
            }
            if existing.position.is_none() {
                existing.position = r.position;
            }
            existing.is_primary = existing.is_primary || primary;
        } else if keys.insert(key) {
            groups.push(SeriesGroup {
                display_name: trimmed.to_owned(),
                asin: r
                    .series_asin
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                position: r.position,
                is_primary: primary,
            });
        }
    }

    if groups.is_empty() {
        return Ok(0);
    }

    // Resolve each group to a series_id BEFORE touching the
    // junction (matches the narrators flow). Pending-row series
    // (ambiguous name + no corroboration winner) are skipped;
    // pending rows surface for operator resolution via the H.3.6
    // surface.
    let mut entries: Vec<(i64, Option<f64>, bool)> = Vec::with_capacity(groups.len());
    for g in &groups {
        if let Some(sid) =
            find_or_insert_series(tx, &g.display_name, g.asin.as_deref(), book_id).await?
        {
            entries.push((sid, g.position, g.is_primary));
        } else {
            tracing::info!(
                book = %book_id,
                alias = %g.display_name,
                "identity.series.deferred_pending"
            );
        }
    }

    // Clear-then-add the junction. Same shape as resolve_narrators.
    sqlx::query!("DELETE FROM book_series WHERE book_id = ?", id)
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity clear book_series: {e}")))?;
    for (sid, position, is_primary) in &entries {
        let primary_i = i64::from(*is_primary);
        sqlx::query!(
            "INSERT INTO book_series (book_id, series_id, position, is_primary) \
             VALUES (?, ?, ?, ?)",
            id,
            sid,
            position,
            primary_i,
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity insert book_series: {e}")))?;
    }

    // Slice 10B: recompute franchise prefix for every series this
    // book now belongs to. Cheap (series have ≤ tens of members
    // typically); always-on so a new book joining a series
    // refreshes the prefix without an explicit verb.
    crate::franchise::recompute_franchise_for_book(tx, id).await?;

    Ok(entries.len())
}

/// Find-or-insert a series row by `audible_id` (when supplied)
/// with case-insensitive name fallback. Back-fills
/// `series.audible_id` on an existing name-matched row when the
/// caller now has an ASIN (matches the `find_or_insert_person`
/// shape used for authors / narrators).
async fn find_or_insert_series(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    name: &str,
    audible_id: Option<&str>,
    book_id: BookId,
) -> Result<Option<i64>> {
    // 1) ASIN match wins when we have one.
    if let Some(asin) = audible_id {
        let existing: Option<i64> = sqlx::query_scalar!(
            "SELECT series_id FROM series WHERE audible_id = ? LIMIT 1",
            asin,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity lookup series by asin: {e}")))?;
        if let Some(id) = existing {
            register_alias_for_kind(tx, "series", id, name, alias_source(audible_id)).await?;
            return Ok(Some(id));
        }
    }

    // 2) Alias-junction lookup with corroboration on multi-hit.
    let alias_matches = lookup_by_alias(tx, "series", "series_id", name).await?;
    match alias_matches.as_slice() {
        [] => {}
        [single] => {
            let id = *single;
            if let Some(asin) = audible_id {
                sqlx::query!(
                    "UPDATE series SET audible_id = ? \
                     WHERE series_id = ? AND audible_id IS NULL",
                    asin,
                    id,
                )
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    Error::Database(format!("identity backfill series audible_id: {e}"))
                })?;
            }
            register_alias_for_kind(tx, "series", id, name, alias_source(audible_id)).await?;
            return Ok(Some(id));
        }
        ambiguous => {
            let scores = corroborate_for_kind(tx, "series", book_id, ambiguous).await?;
            if let Some(winner) = pick_corroborated(&scores) {
                tracing::info!(
                    table = "series",
                    alias = name,
                    winner,
                    candidates = ?ambiguous,
                    "identity.match.corroborated"
                );
                if let Some(asin) = audible_id {
                    sqlx::query!(
                        "UPDATE series SET audible_id = ? \
                         WHERE series_id = ? AND audible_id IS NULL",
                        asin,
                        winner,
                    )
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| {
                        Error::Database(format!("identity backfill series audible_id: {e}"))
                    })?;
                }
                register_alias_for_kind(tx, "series", winner, name, alias_source(audible_id))
                    .await?;
                return Ok(Some(winner));
            }
            write_pending_disambiguation(tx, "series", book_id, name, &scores).await?;
            return Ok(None);
        }
    }

    // 3) Insert new row + canonical alias.
    let new_id: i64 = sqlx::query_scalar!(
        "INSERT INTO series (name, audible_id) VALUES (?, ?) RETURNING series_id",
        name,
        audible_id,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("identity insert series: {e}")))?;
    insert_alias_with_flag(tx, "series", new_id, name, "canonical", true).await?;
    Ok(Some(new_id))
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
    // set before touching the junction. Pending-row narrators (a
    // name maps ambiguously to multiple parents AND corroboration
    // doesn't decide) are skipped — the book gets the
    // unambiguous narrators only; the operator resolves the
    // pending ones via `aborg names resolve`.
    let mut narrator_ids: Vec<i64> = Vec::with_capacity(candidates.len());
    for c in &candidates {
        if let Some(nid) = find_or_insert_person(
            tx,
            "narrators",
            "narrator_id",
            &c.name,
            c.external_id.as_deref(),
            book_id,
        )
        .await?
        {
            narrator_ids.push(nid);
        } else {
            tracing::info!(
                book = %book_id,
                alias = %c.name,
                "identity.narrator.deferred_pending"
            );
        }
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
#[allow(clippy::too_many_arguments)] // 5 inputs + tx + book_id — bundling adds indirection that obscures the dispatch
async fn find_or_insert_person(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    id_column: &'static str,
    name: &str,
    external_id: Option<&str>,
    book_id: BookId,
) -> Result<Option<i64>> {
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
            register_alias_for_kind(tx, table, id, name, alias_source(external_id)).await?;
            return Ok(Some(id));
        }
    }
    // 2) Alias-junction lookup. Multi-row hits go through the
    //    corroboration pass (H.3.5).
    let alias_matches = lookup_by_alias(tx, table, id_column, name).await?;
    match alias_matches.as_slice() {
        [] => {}
        [single] => {
            let id = *single;
            // Back-fill audible_id when we have one and the row
            // doesn't.
            if let Some(ext) = external_id {
                let update_sql = format!(
                    "UPDATE {table} SET audible_id = ? \
                     WHERE {id_column} = ? AND audible_id IS NULL"
                );
                sqlx::query(&update_sql)
                    .bind(ext)
                    .bind(id)
                    .execute(&mut **tx)
                    .await
                    .map_err(|e| Error::Database(format!("identity backfill {table}: {e}")))?;
            }
            register_alias_for_kind(tx, table, id, name, alias_source(external_id)).await?;
            return Ok(Some(id));
        }
        ambiguous => {
            // Corroborate. If a clear winner emerges, attach;
            // otherwise write a pending row and leave the FK NULL.
            let scores = corroborate_for_kind(tx, table, book_id, ambiguous).await?;
            if let Some(winner) = pick_corroborated(&scores) {
                tracing::info!(
                    table,
                    alias = name,
                    winner,
                    candidates = ?ambiguous,
                    "identity.match.corroborated"
                );
                if let Some(ext) = external_id {
                    let update_sql = format!(
                        "UPDATE {table} SET audible_id = ? \
                         WHERE {id_column} = ? AND audible_id IS NULL"
                    );
                    sqlx::query(&update_sql)
                        .bind(ext)
                        .bind(winner)
                        .execute(&mut **tx)
                        .await
                        .map_err(|e| Error::Database(format!("identity backfill {table}: {e}")))?;
                }
                register_alias_for_kind(tx, table, winner, name, alias_source(external_id)).await?;
                return Ok(Some(winner));
            }
            write_pending_disambiguation(tx, table, book_id, name, &scores).await?;
            return Ok(None);
        }
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
    // Seed the canonical alias for the new parent.
    insert_alias_with_flag(tx, table, new_id, name, "canonical", true).await?;
    Ok(Some(new_id))
}

/// Map a candidate's `external_id` presence to the alias `source`
/// closed-vocab string per ADR-0026.
const fn alias_source(external_id: Option<&str>) -> &'static str {
    if external_id.is_some() {
        "audnexus"
    } else {
        "tag_file"
    }
}

/// Look up parent IDs whose alias junction contains `alias`
/// (case-insensitive via the junction's `COLLATE NOCASE` index).
/// Returns every match — caller decides how to handle multi-row
/// hits.
///
/// `table` is the closed-allowlist parent name (`authors` /
/// `narrators` / `series`); the junction is derived by stripping
/// the trailing `s` and appending `_aliases`. SQL is built with
/// `format!` because the literal-only `sqlx::query!` can't
/// dispatch across three tables. The values come from a static
/// allowlist so injection isn't reachable here.
async fn lookup_by_alias(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    id_column: &'static str,
    alias: &str,
) -> Result<Vec<i64>> {
    let (junction, parent_fk) = junction_for(table);
    let sql = format!(
        "SELECT {parent_fk} FROM {junction} \
         WHERE alias = ? COLLATE NOCASE"
    );
    let rows: Vec<i64> = sqlx::query_scalar(&sql)
        .bind(alias)
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity alias lookup {table}: {e}")))?;
    // `id_column` is shadowed by the FK column we actually select;
    // keep the param so the call site stays symmetric with
    // `find_or_insert_person`.
    let _ = id_column;
    Ok(rows)
}

/// Register an observed alias on an existing parent row. Idempotent
/// via the junction's `UNIQUE (parent_id, alias)` constraint —
/// repeat insertions for the same `(parent, alias)` pair are no-ops.
/// Never sets `is_prime`; manual exaltation goes through the
/// dedicated H.3.4 surface.
async fn register_alias_for_kind(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    parent_id: i64,
    alias: &str,
    source: &'static str,
) -> Result<()> {
    insert_alias_with_flag(tx, table, parent_id, alias, source, false).await
}

/// Lower-level junction insert. `is_prime` true is only used at
/// canonical-insert time; the partial unique index would reject a
/// second prime row.
#[allow(clippy::too_many_arguments)] // 5 inputs + tx + is_prime — bundling either adds an indirection that obscures the call site
async fn insert_alias_with_flag(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    parent_id: i64,
    alias: &str,
    source: &'static str,
    is_prime: bool,
) -> Result<()> {
    let (junction, parent_fk) = junction_for(table);
    let prime: i64 = i64::from(is_prime);
    let sql = format!(
        "INSERT OR IGNORE INTO {junction} \
         ({parent_fk}, alias, source, is_prime) VALUES (?, ?, ?, ?)"
    );
    sqlx::query(&sql)
        .bind(parent_id)
        .bind(alias)
        .bind(source)
        .bind(prime)
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("identity alias insert {table}: {e}")))?;
    Ok(())
}

/// Closed-allowlist dispatch from the parent table name to its
/// `_aliases` junction + the FK column inside that junction.
/// Panics on an unknown table; only callers in this module
/// supply the name, so the panic-arm is unreachable in practice.
fn junction_for(table: &'static str) -> (&'static str, &'static str) {
    match table {
        "authors" => ("author_aliases", "author_id"),
        "narrators" => ("narrator_aliases", "narrator_id"),
        "series" => ("series_aliases", "series_id"),
        // The allowlist is closed; this branch exists for the
        // compiler. A new identity kind has to register its
        // junction here before the helpers will dispatch.
        other => unreachable!("junction_for: unknown identity table {other}"),
    }
}

/// Disambiguation pending-table dispatch.
fn pending_tables_for(
    table: &'static str,
) -> (&'static str, &'static str, &'static str, &'static str) {
    // (pending_table, candidate_table, parent_fk_in_pending,
    // candidate_fk_in_candidate)
    match table {
        "authors" => (
            "author_disambiguation_pending",
            "author_disambiguation_candidate",
            "resolved_author_id",
            "author_id",
        ),
        "narrators" => (
            "narrator_disambiguation_pending",
            "narrator_disambiguation_candidate",
            "resolved_narrator_id",
            "narrator_id",
        ),
        "series" => (
            "series_disambiguation_pending",
            "series_disambiguation_candidate",
            "resolved_series_id",
            "series_id",
        ),
        other => unreachable!("pending_tables_for: unknown identity table {other}"),
    }
}

/// Minimum winner-vs-runner-up margin for a corroborated match to
/// auto-attach. Below this → pending row + NULL FK. Conservative
/// default; tuneable once the pending table has real entries
/// (ADR-0026 verification section).
const CORROBORATION_MARGIN: f64 = 0.3;

/// Per-candidate score from the corroboration pass. Returned to the
/// caller so the pending-row write can persist it for the resolve
/// UI ("candidate A scored 0.45, candidate B scored 0.42").
#[derive(Debug, Clone, Copy)]
struct CandidateScore {
    id: i64,
    score: f64,
}

/// Disambiguate ambiguous author candidates. Signals are narrator
/// overlap (cap 0.4), publisher overlap (0.2), series-author
/// overlap via the book's `book_series` rows (0.5). Series carries
/// the strongest weight because if we know the series and the
/// series' other books point to one author, that's a near-certain
/// match.
async fn corroborate_author(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    candidates: &[i64],
) -> Result<Vec<CandidateScore>> {
    let mut scores: Vec<CandidateScore> = Vec::with_capacity(candidates.len());
    for &cand in candidates {
        let mut s = 0.0_f64;
        // Narrator overlap. `book_narrator` for this book ∩
        // narrators on the candidate's other books. Distinct count
        // capped at 1.0 × 0.4 = 0.4.
        let id = book_id.0;
        let narrator_overlap: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(DISTINCT bn.narrator_id) AS "n!: i64"
               FROM book_narrator bn
               WHERE bn.book_id = ?
                 AND bn.narrator_id IN (
                     SELECT bn2.narrator_id
                     FROM book_narrator bn2
                     JOIN books b2 ON b2.book_id = bn2.book_id
                     WHERE b2.author_id = ? AND b2.book_id <> ?
                 )"#,
            id,
            cand,
            id,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("corroborate author narrator-overlap: {e}")))?;
        if narrator_overlap > 0 {
            s += 0.4;
        }
        // Publisher overlap.
        let publisher_overlap: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64"
               FROM books b
               WHERE b.book_id = ?
                 AND b.publisher_id IS NOT NULL
                 AND b.publisher_id IN (
                     SELECT b2.publisher_id FROM books b2
                     WHERE b2.author_id = ? AND b2.book_id <> ?
                       AND b2.publisher_id IS NOT NULL
                 )"#,
            id,
            cand,
            id,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("corroborate author publisher-overlap: {e}")))?;
        if publisher_overlap > 0 {
            s += 0.2;
        }
        // Series-author overlap: book is in some series; that
        // series' other books credit this candidate as author.
        let series_overlap: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(DISTINCT bs.series_id) AS "n!: i64"
               FROM book_series bs
               WHERE bs.book_id = ?
                 AND bs.series_id IN (
                     SELECT bs2.series_id
                     FROM book_series bs2
                     JOIN books b2 ON b2.book_id = bs2.book_id
                     WHERE b2.author_id = ? AND b2.book_id <> ?
                 )"#,
            id,
            cand,
            id,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("corroborate author series-overlap: {e}")))?;
        if series_overlap > 0 {
            s += 0.5;
        }
        scores.push(CandidateScore { id: cand, score: s });
    }
    Ok(scores)
}

/// Disambiguate ambiguous narrator candidates. Single signal:
/// publisher overlap. Narrators don't share series memberships
/// reliably enough for that signal to count.
async fn corroborate_narrator(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    candidates: &[i64],
) -> Result<Vec<CandidateScore>> {
    let mut scores: Vec<CandidateScore> = Vec::with_capacity(candidates.len());
    for &cand in candidates {
        let mut s = 0.0_f64;
        let id = book_id.0;
        let publisher_overlap: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64"
               FROM books b
               WHERE b.book_id = ?
                 AND b.publisher_id IS NOT NULL
                 AND b.publisher_id IN (
                     SELECT b2.publisher_id
                     FROM book_narrator bn2
                     JOIN books b2 ON b2.book_id = bn2.book_id
                     WHERE bn2.narrator_id = ? AND b2.book_id <> ?
                       AND b2.publisher_id IS NOT NULL
                 )"#,
            id,
            cand,
            id,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("corroborate narrator publisher-overlap: {e}")))?;
        if publisher_overlap > 0 {
            s += 0.2;
        }
        scores.push(CandidateScore { id: cand, score: s });
    }
    Ok(scores)
}

/// Disambiguate ambiguous series candidates. Signal: this book's
/// resolved author (if any) matches the dominant author across the
/// candidate series' other books. +0.6 weight — series sharing one
/// author across most members is the strongest single signal we
/// have.
async fn corroborate_series(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: BookId,
    candidates: &[i64],
) -> Result<Vec<CandidateScore>> {
    let mut scores: Vec<CandidateScore> = Vec::with_capacity(candidates.len());
    let id = book_id.0;
    let this_author: Option<i64> =
        sqlx::query_scalar!("SELECT author_id FROM books WHERE book_id = ?", id,)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| Error::Database(format!("corroborate series read author: {e}")))?;
    for &cand in candidates {
        let mut s = 0.0_f64;
        if let Some(author_id) = this_author {
            let overlap: i64 = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "n!: i64"
                   FROM book_series bs
                   JOIN books b ON b.book_id = bs.book_id
                   WHERE bs.series_id = ? AND b.author_id = ?
                     AND bs.book_id <> ?"#,
                cand,
                author_id,
                id,
            )
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| Error::Database(format!("corroborate series author-overlap: {e}")))?;
            if overlap > 0 {
                s += 0.6;
            }
        }
        scores.push(CandidateScore { id: cand, score: s });
    }
    Ok(scores)
}

/// Pick a winner from a scored candidate set if the margin
/// (best − runner-up) exceeds [`CORROBORATION_MARGIN`]. Returns
/// `None` to mean "ambiguous; write pending."
fn pick_corroborated(scores: &[CandidateScore]) -> Option<i64> {
    let mut sorted: Vec<&CandidateScore> = scores.iter().collect();
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    match sorted.as_slice() {
        [] => None,
        [only] => Some(only.id),
        [best, runner_up, ..] => {
            if best.score - runner_up.score > CORROBORATION_MARGIN {
                Some(best.id)
            } else {
                None
            }
        }
    }
}

/// Write a pending-disambiguation row + candidate scores so the
/// operator can resolve via `aborg names resolve` (H.3.6).
/// Idempotent via the pending table's
/// `UNIQUE (book_id, observed_alias)`.
async fn write_pending_disambiguation(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    book_id: BookId,
    observed_alias: &str,
    scores: &[CandidateScore],
) -> Result<()> {
    let (pending_table, candidate_table, _resolved_col, candidate_fk) = pending_tables_for(table);
    let id = book_id.0;
    let insert_pending = format!(
        "INSERT OR IGNORE INTO {pending_table} (book_id, observed_alias) \
         VALUES (?, ?) RETURNING pending_id"
    );
    let pending_id: Option<i64> = sqlx::query_scalar(&insert_pending)
        .bind(id)
        .bind(observed_alias)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("write pending {pending_table}: {e}")))?;
    let Some(pending_id) = pending_id else {
        // Row already present (idempotent re-run); don't re-stamp
        // candidates — they'd repeat with stale scores. Future
        // corroboration sweeps can prune.
        return Ok(());
    };
    let insert_candidate = format!(
        "INSERT OR IGNORE INTO {candidate_table} \
         (pending_id, {candidate_fk}, score) VALUES (?, ?, ?)"
    );
    for s in scores {
        sqlx::query(&insert_candidate)
            .bind(pending_id)
            .bind(s.id)
            .bind(s.score)
            .execute(&mut **tx)
            .await
            .map_err(|e| Error::Database(format!("write pending {candidate_table}: {e}")))?;
    }
    tracing::info!(
        table,
        book = id,
        alias = observed_alias,
        candidates = scores.len(),
        "identity.disambiguation.pending"
    );
    Ok(())
}

/// Dispatch the corroboration pass to the kind-appropriate helper.
async fn corroborate_for_kind(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    book_id: BookId,
    candidates: &[i64],
) -> Result<Vec<CandidateScore>> {
    match table {
        "authors" => corroborate_author(tx, book_id, candidates).await,
        "narrators" => corroborate_narrator(tx, book_id, candidates).await,
        "series" => corroborate_series(tx, book_id, candidates).await,
        other => unreachable!("corroborate_for_kind: unknown identity table {other}"),
    }
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
            stage_name: "resolve-identity",
        }
    }

    #[tokio::test]
    async fn stage_metadata_matches_pipeline_expectations() {
        let stage = IdentityResolveStage::new();
        assert_eq!(stage.name(), "resolve-identity");
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
             (book_id, field, value, source, stage, confidence) \
             VALUES \
               (1, 'author', 'Brandon Sanderson', 'audnexus_asin_us', 'enrich-from-audnexus', 0.95), \
               (1, 'author', 'brandon sanderson', 'tag_file',         'read-tags',        0.7), \
               (1, 'publisher', 'Recorded Books', 'audnexus_asin_us', 'enrich-from-audnexus', 0.95), \
               (1, 'narrator', 'Michael Kramer',  'audnexus_asin_us', 'enrich-from-audnexus', 0.95), \
               (1, 'narrator', 'Kate Reading',    'audnexus_asin_us', 'enrich-from-audnexus', 0.95)",
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
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'narrator', 'Original Reader', 'tag_file', 'read-tags', 0.7)",
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
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'narrator', 'New Reader', 'audnexus_asin_us', 'enrich-from-audnexus', 0.95)",
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
             (book_id, field, value, source, stage, confidence, external_id) \
             VALUES \
               (1, 'author', 'Haruki Murakami',  'audnexus_asin_us', 'enrich-from-audnexus', 0.95, 'B0AUTHORX'), \
               (2, 'author', 'Murakami, Haruki', 'audnexus_asin_jp', 'enrich-from-audnexus', 0.95, 'B0AUTHORX')",
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
    async fn alias_junction_seeded_on_insert_and_observation() {
        // Single book, two provenance rows: Audnexus brings the
        // ASIN-stamped canonical, read-tags brings a variant
        // spelling. Result: one `authors` row, two
        // `author_aliases` rows — canonical (is_prime=1) and the
        // variant (is_prime=0).
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Book')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence, external_id) \
             VALUES \
               (1, 'author', 'Brandon Sanderson', 'audnexus_asin_us', 'enrich-from-audnexus', 0.95, 'B0SANDXYZ'), \
               (1, 'author', 'brandon sanderson', 'tag_file',         'read-tags',        0.7,  NULL)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        IdentityResolveStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");

        // One author row.
        let author_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM authors")
            .fetch_one(ctx.library.pool())
            .await
            .expect("count authors");
        assert_eq!(author_count, 1);

        // Canonical alias from the canonical insertion path. The
        // case-insensitive winner ("Brandon Sanderson") becomes
        // the canonical; the tag-derived "brandon sanderson"
        // doesn't appear as a distinct alias because the
        // dedup-on-lower-name in `fetch_all_distinct` and the
        // junction's `UNIQUE (author_id, alias)` together filter
        // it before it lands.
        let canonical_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM author_aliases \
             WHERE is_prime = 1 AND source = 'canonical'",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("canonical count");
        assert_eq!(canonical_count, 1);

        let canonical_alias: String =
            sqlx::query_scalar("SELECT alias FROM author_aliases WHERE is_prime = 1 LIMIT 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("canonical alias");
        assert_eq!(canonical_alias, "Brandon Sanderson");
    }

    #[tokio::test]
    async fn second_distinct_spelling_lands_as_non_prime_alias() {
        // Two books, two different spellings for the same
        // Audnexus-ASIN'd author. Both books should attach to
        // one author row (ASIN match collapses); both spellings
        // should appear as alias rows; exactly one is the prime.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'A'), (2, 'B')")
            .execute(ctx.library.pool())
            .await
            .expect("seed books");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence, external_id) \
             VALUES \
               (1, 'author', 'Haruki Murakami',  'audnexus_asin_us', 'enrich-from-audnexus', 0.95, 'B0AUTHOR'), \
               (2, 'author', 'Murakami, Haruki', 'audnexus_asin_jp', 'enrich-from-audnexus', 0.95, 'B0AUTHOR')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed");

        IdentityResolveStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run 1");
        IdentityResolveStage::new()
            .run(&ctx, BookId(2))
            .await
            .expect("run 2");

        // Exactly one author row, two alias rows, one prime.
        let alias_rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT alias, is_prime FROM author_aliases ORDER BY alias")
                .fetch_all(ctx.library.pool())
                .await
                .expect("read aliases");
        assert_eq!(alias_rows.len(), 2, "two alias spellings recorded");
        let prime_count = alias_rows.iter().filter(|r| r.1 == 1).count();
        assert_eq!(prime_count, 1, "exactly one prime alias");
    }

    #[tokio::test]
    async fn ambiguous_alias_with_no_corroboration_lands_in_pending() {
        // Two David Mitchells with different ASINs already in the
        // DB. A new book credits "David Mitchell" with no ASIN and
        // no corroborating signal (no narrator/publisher/series
        // overlap). Result: pending row + null books.author_id;
        // both candidates recorded with score 0.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query(
            "INSERT INTO authors (name, audible_id) VALUES \
                 ('David Mitchell', 'B0034Q40L2'), \
                 ('David Mitchell', 'B000APTQBE')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed two David Mitchells");
        sqlx::query(
            "INSERT INTO author_aliases (author_id, alias, source, is_prime) VALUES \
                 (1, 'David Mitchell', 'canonical', 1), \
                 (2, 'David Mitchell', 'canonical', 1)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed aliases");
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Mystery Title')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'author', 'David Mitchell', 'tag_file', 'read-tags', 0.7)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        IdentityResolveStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");

        // books.author_id should still be NULL — pending.
        let author_id: Option<Option<i64>> =
            sqlx::query_scalar("SELECT author_id FROM books WHERE book_id = 1")
                .fetch_optional(ctx.library.pool())
                .await
                .expect("read author_id");
        assert_eq!(
            author_id.flatten(),
            None,
            "ambiguous match should leave author_id NULL"
        );

        // Pending row exists for the book.
        let pending_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM author_disambiguation_pending WHERE book_id = 1",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("count pending");
        assert_eq!(pending_count, 1);

        // Both candidates recorded with score 0 (no corroboration
        // signal landed).
        let cand_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM author_disambiguation_candidate")
                .fetch_one(ctx.library.pool())
                .await
                .expect("count candidates");
        assert_eq!(cand_count, 2);
    }

    #[tokio::test]
    async fn ambiguous_alias_with_narrator_overlap_corroborates() {
        // Two David Mitchells; book has a known narrator that's
        // shared with one of the David Mitchells' other books.
        // Corroboration should pick that David Mitchell + no
        // pending row.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        // Seed two authors, two narrators, three books.
        sqlx::query(
            "INSERT INTO authors (name, audible_id) VALUES \
                 ('David Mitchell', 'B0034Q40L2'), \
                 ('David Mitchell', 'B000APTQBE')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed authors");
        sqlx::query(
            "INSERT INTO author_aliases (author_id, alias, source, is_prime) VALUES \
                 (1, 'David Mitchell', 'canonical', 1), \
                 (2, 'David Mitchell', 'canonical', 1)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed aliases");
        sqlx::query(
            "INSERT INTO narrators (narrator_id, name) VALUES \
                 (10, 'Narrator One'), (20, 'Narrator Two')",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed narrators");
        // Book by author 1 (David Mitchell A) narrated by 10.
        sqlx::query(
            "INSERT INTO books (book_id, title, author_id) VALUES \
                 (100, 'Existing A Book', 1), \
                 (200, 'Existing B Book', 2)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed existing books");
        sqlx::query(
            "INSERT INTO book_narrator (book_id, narrator_id) VALUES \
                 (100, 10), (200, 20)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed narrator links");

        // New book with narrator 10 → should resolve to author 1.
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'New Book')")
            .execute(ctx.library.pool())
            .await
            .expect("seed target book");
        // Wire its narrator before resolve-identity runs (in real
        // pipeline that's `enrich-from-audnexus`'s job; we simulate).
        sqlx::query("INSERT INTO book_narrator (book_id, narrator_id) VALUES (1, 10)")
            .execute(ctx.library.pool())
            .await
            .expect("seed target narrator");
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'author', 'David Mitchell', 'tag_file', 'read-tags', 0.7)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");

        IdentityResolveStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");

        // Should resolve to author 1 (the one whose existing book
        // shares narrator 10).
        let author_id: Option<i64> =
            sqlx::query_scalar("SELECT author_id FROM books WHERE book_id = 1")
                .fetch_one(ctx.library.pool())
                .await
                .expect("read author_id");
        assert_eq!(author_id, Some(1), "narrator overlap should disambiguate");

        // No pending row.
        let pending_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM author_disambiguation_pending WHERE book_id = 1",
        )
        .fetch_one(ctx.library.pool())
        .await
        .expect("count pending");
        assert_eq!(pending_count, 0);
    }

    #[tokio::test]
    async fn name_match_back_fills_audible_id_on_later_run() {
        // First run: read-tags inserted "Brandon Sanderson" without
        // an ASIN. Second run: enrich-from-audnexus brings the ASIN.
        // The existing row should get its `audible_id` filled in,
        // not a new row created.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path()).await;
        sqlx::query("INSERT INTO books (book_id, title) VALUES (1, 'Book A')")
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
        // Run 1: read-tags style, no external_id.
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence) \
             VALUES (1, 'author', 'Brandon Sanderson', 'tag_file', 'read-tags', 0.7)",
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

        // Run 2: enrich-from-audnexus brings the ASIN. Append candidate.
        sqlx::query(
            "INSERT INTO book_field_provenance \
             (book_id, field, value, source, stage, confidence, external_id) \
             VALUES (1, 'author', 'Brandon Sanderson', 'audnexus_asin_us', 'enrich-from-audnexus', 0.95, 'B0SANDXYZ')",
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
