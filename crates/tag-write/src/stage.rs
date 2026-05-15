//! `tag-write-early` + `tag-write-final` Stage impls (ADR-0028).
//!
//! Both stages share the per-book write pattern: load winners
//! from `book_field_provenance`, derive a per-run `batch_id`,
//! fetch the cover-art bytes once when a `CoverUrl` winner is
//! present, then loop over every active `book_files` row
//! calling [`crate::write::write_winners`] under a
//! `book_file_refs` lifecycle guard (ADR-0027). Per-field
//! before/after pairs flow into `mass_edit_history`.
//!
//! The stages differ in two ways:
//!
//! - **When they run.** Early sits right after `tag-read` +
//!   `identity-resolve` + `extract-dna-tags`; Final sits after
//!   every AI extractor (`extract-summary-spoiler-free`,
//!   `extract-story-arc`, `extract-characters`, `extract-setting`,
//!   `extract-summary-spoiler-free-series`) plus `consensus` and
//!   `transcode-m4b`. The `requires()` lists encode the
//!   ordering.
//! - **What they filter.** Early writes every available winner.
//!   Final additionally strips `source = 'user_edit'` winners
//!   (ADR-0028 § "Skips per-field on user-edit") so a user
//!   correction made between import and AI completion stays
//!   sticky.
//!
//! Both are gated behind separate `PipelineTunables` switches —
//! `tag_write_early_enabled` + `tag_write_final_enabled` — and
//! both default `false`. Flipping either on re-tags every book
//! whose winners differ from on-disk; that's a deliberate
//! operator decision, not a fresh-checkout default. Daemon
//! wiring lives in `bins/aborg-daemon/src/main.rs` §
//! `build_pipeline_stages`.

use async_trait::async_trait;

use ab_core::{BookId, Error, FileId, Result};
use ab_db::book_file_refs;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};
use uuid::Uuid;

use crate::USER_EDIT_SOURCE;
use crate::winners::{FieldWinner, select_winners_for_book};
use crate::write::{FieldChange, WriteReport, write_winners};

/// Typed stage identifier for the early tag-write pass.
pub const TAG_WRITE_EARLY_STAGE_ID: StageId = StageId::new("tag-write-early");
/// Stable `&'static str` mirror of [`TAG_WRITE_EARLY_STAGE_ID`].
pub const TAG_WRITE_EARLY_STAGE_NAME: &str = TAG_WRITE_EARLY_STAGE_ID.as_str();

/// Typed stage identifier for the final tag-write pass.
pub const TAG_WRITE_FINAL_STAGE_ID: StageId = StageId::new("tag-write-final");
/// Stable `&'static str` mirror of [`TAG_WRITE_FINAL_STAGE_ID`].
pub const TAG_WRITE_FINAL_STAGE_NAME: &str = TAG_WRITE_FINAL_STAGE_ID.as_str();

/// `Stage::requires` set for the early pass.
///
/// Per ADR-0028: `tag-read`, `identity-resolve`, `extract-dna-tags`.
/// Only `tag-read` exists as a referenceable `StageId` today; the
/// other two land on their owning crates' typed-`StageId` slices
/// and get appended here. The scheduler treats `Skipped` outcomes
/// as satisfied so partial lists don't deadlock.
const TAG_WRITE_EARLY_REQUIRES: &[StageId] = &[StageId::new("tag-read")];

/// `Stage::requires` set for the final pass.
///
/// Per ADR-0028 § "`TagWriteFinal` `requires()`": every AI extractor
/// that can produce a `book_field_provenance` row, plus
/// `transcode-m4b` so the late write lands on the post-transcode
/// canonical file rather than the soon-to-be-deleted source. All
/// listed stages are unconditionally registered by the daemon
/// (`bins/aborg-daemon/src/main.rs` § `build_pipeline_stages`); the
/// `Dag::build` topological-sort step verifies presence at boot.
///
/// Stage names are bare strings rather than imported typed
/// constants to avoid pulling `ab-llm-extractors`, `ab-catalog`,
/// and `ab-transcode` into `ab-tag-write`'s compile graph. A
/// stage rename would surface as a `Dag::build` "unknown
/// dependency" error at daemon startup — a fast failure.
const TAG_WRITE_FINAL_REQUIRES: &[StageId] = &[
    StageId::new("tag-read"),
    StageId::new("promote-consensus"),
    StageId::new("extract-summary-spoiler-free"),
    StageId::new("extract-story-arc"),
    StageId::new("extract-characters"),
    StageId::new("extract-setting"),
    StageId::new("extract-summary-spoiler-free-series"),
    StageId::new("transcode-m4b"),
];

/// Early-pass tag-write stage (ADR-0028 § `TagWriteEarly`).
///
/// Intended priority: `Foreground`. Writes the supported subset
/// of the 16 `book_field_provenance` fields (Title, Author,
/// Series, Language, Genre, Publisher, Asin, Isbn — the rest
/// land in a follow-up slice per
/// [`crate::write`]'s coverage table) using the `is_winner = 1`
/// row for each field. Skips when every winner is itself from
/// `source = 'tag_file'` (no point writing tags we just read).
///
/// Carries a [`crate::CoverClient`] so the per-book run can
/// fetch the cover-art payload once (when a `CoverUrl` winner
/// exists) and reuse the bytes across every active
/// `book_files` row.
#[derive(Debug, Clone)]
pub struct TagWriteEarlyStage {
    cover: crate::CoverClient,
}

impl TagWriteEarlyStage {
    /// Construct from the workspace's HTTP-client tunables. The
    /// `CoverClient` build can fail on a corrupt TLS stack;
    /// callers may use [`Self::new`] for the default-tunables
    /// path which falls back to a permissive client.
    ///
    /// # Errors
    ///
    /// Surfaces [`crate::CoverFetchError::ClientBuild`] from
    /// the underlying `reqwest::Client::build`.
    pub fn from_tunables(
        tunables: &ab_core::tunables::HttpClientTunables,
    ) -> std::result::Result<Self, crate::CoverFetchError> {
        Ok(Self {
            cover: crate::CoverClient::new(tunables)?,
        })
    }

    /// Construct with the default HTTP-client tunables. Returns
    /// a `Self` even on `CoverClient` build failure — the
    /// cover-fetch path then surfaces every URL as a transient
    /// `Request` error at run-time, which the stage logs +
    /// continues past. Choice rationale: cover-art is best-effort,
    /// not a daemon-startup blocker.
    #[must_use]
    pub fn new() -> Self {
        let defaults = ab_core::tunables::HttpClientTunables::default();
        match crate::CoverClient::new(&defaults) {
            Ok(cover) => Self { cover },
            Err(e) => {
                tracing::warn!(error = %e, "tag-write.cover_client_build_failed_using_fallback");
                // Best-effort fallback: build with an even more
                // permissive client builder that always succeeds
                // (no custom timeouts). We can't propagate the
                // error without breaking the `Default` contract,
                // and a daemon that won't boot because cover-art
                // is misconfigured isn't acceptable.
                let http = reqwest::Client::new();
                Self {
                    cover: ab_covers::CoverClient::with_parts(http, defaults.cover_max_bytes),
                }
            }
        }
    }
}

impl Default for TagWriteEarlyStage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Stage for TagWriteEarlyStage {
    fn name(&self) -> &'static str {
        TAG_WRITE_EARLY_STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        TAG_WRITE_EARLY_REQUIRES
    }

    #[allow(
        clippy::too_many_lines,
        reason = "per-book linear flow: load winners → skip checks → cover fetch → per-file loop → summary log. Splitting hurts readability — the steps are sequential and the early-returns make the function easy to follow top-to-bottom."
    )]
    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let winners = select_winners_for_book(ctx.library.pool(), book_id.0).await?;
        if winners.is_empty() {
            tracing::debug!(
                book = %book_id,
                stage = TAG_WRITE_EARLY_STAGE_NAME,
                "tag-write.early.no_winners"
            );
            return Ok(StageOutcome::Skipped);
        }

        // ADR-0028 § "Skips": when every available winner is
        // itself from `source = 'tag_file'`, writing them is a
        // tautology — we'd just be persisting what we read. Skip
        // entirely. (`user_edit` rows count as non-tag_file so
        // they survive the filter for the late pass; Early
        // doesn't differentiate by source, only by tautology.)
        if winners.iter().all(|w| w.source == "tag_file") {
            tracing::debug!(
                book = %book_id,
                stage = TAG_WRITE_EARLY_STAGE_NAME,
                winner_count = winners.len(),
                "tag-write.early.all_winners_from_tag_file_skip"
            );
            return Ok(StageOutcome::Skipped);
        }

        let files = load_active_file_paths(ctx, book_id).await?;
        if files.is_empty() {
            tracing::debug!(
                book = %book_id,
                stage = TAG_WRITE_EARLY_STAGE_NAME,
                "tag-write.early.no_active_files"
            );
            return Ok(StageOutcome::Skipped);
        }

        // The skip-list constant from `crate` keeps the "what
        // does user_edit mean" knowledge in one place even
        // though Early doesn't use it for filtering — the
        // import is the trail-leaving move that helps the
        // future final-stage author find the constant.
        let _ = USER_EDIT_SOURCE;

        // Cover-art fetch: do it once before the per-file
        // loop so multi-file books share a single HTTP
        // round-trip. Failures here are best-effort — log +
        // skip the cover field; the other winners still write.
        let cover_bytes: Option<Vec<u8>> = match winners
            .iter()
            .find(|w| w.field == ab_core::Field::CoverUrl)
            .and_then(|w| w.value.as_deref())
        {
            Some(url) => match self.cover.fetch(url).await {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    tracing::warn!(
                        book = %book_id,
                        url = %url,
                        error = %e,
                        "tag-write.early.cover_fetch_failed"
                    );
                    None
                }
            },
            None => None,
        };

        // One batch_id per Stage::run invocation. Per the
        // `mass_edit_history` schema comment, batch_id "links
        // edits made in one operation" — every per-file
        // per-field row inserted by this run shares the UUID.
        let batch_id = Uuid::new_v4().to_string();
        let mut total_changes: usize = 0;
        let mut total_matched: usize = 0;
        let mut total_unmapped: usize = 0;
        let mut any_changed = false;
        for (file_id, file_path) in files {
            match write_one_file(
                ctx,
                book_id,
                file_id,
                &file_path,
                &winners,
                cover_bytes.as_deref(),
            )
            .await
            {
                Ok(report) => {
                    total_changes += report.fields_changed();
                    total_matched += report.fields_already_matched;
                    total_unmapped += report.fields_unmapped;
                    if !report.changes.is_empty() {
                        any_changed = true;
                        // Mirror each per-field mutation to
                        // mass_edit_history. Per `PROJECT.md`
                        // § Tag-write history: "Every tag write
                        // logs before/after to mass_edit_history."
                        // Failures here are non-fatal (audit-log
                        // gaps shouldn't roll back the on-disk
                        // write) — log and continue.
                        if let Err(e) =
                            record_audit_rows(ctx, &batch_id, file_id, &report.changes).await
                        {
                            tracing::warn!(
                                book = %book_id,
                                file_id = file_id.0,
                                error = %e,
                                "tag-write.early.audit_log_failed"
                            );
                        }
                    }
                }
                Err(e) => {
                    // Per-file errors don't abort the book — a
                    // single broken file shouldn't block the
                    // others. Surface to tracing; the scheduler
                    // will retry the stage on a future pass via
                    // ADR-0023.
                    tracing::warn!(
                        book = %book_id,
                        file_id = file_id.0,
                        path = %file_path,
                        error = %e,
                        "tag-write.early.file_failed"
                    );
                }
            }
        }

        tracing::info!(
            book = %book_id,
            stage = TAG_WRITE_EARLY_STAGE_NAME,
            batch_id = %batch_id,
            fields_changed = total_changes,
            fields_already_matched = total_matched,
            fields_unmapped = total_unmapped,
            "tag-write.early.done"
        );

        if any_changed {
            Ok(StageOutcome::Done)
        } else {
            // Every file was either a no-op match or covered
            // unmapped-only fields. Idempotent re-runs hit this
            // path.
            Ok(StageOutcome::Skipped)
        }
    }
}

/// Insert one `mass_edit_history` row per `FieldChange` for the
/// given file. Each row encodes the before / after string as a
/// JSON-quoted scalar (matching the schema's `-- JSON` column
/// comment) so future structured values (cover art metadata,
/// multi-value tag arrays) can land in the same column without
/// a schema change.
///
/// `batch_id` is the shared UUID for the run. `actor = 'system'`
/// per PROJECT.md § Tag-write history.
async fn record_audit_rows(
    ctx: &StageContext,
    batch_id: &str,
    file_id: FileId,
    changes: &[FieldChange],
) -> Result<()> {
    let target_kind = "book_files";
    let target_id = file_id.0;
    let actor = "system";
    for change in changes {
        let field_str = change.field.as_str();
        // JSON-quote the string scalars. `serde_json` is
        // already in the workspace; rolling a manual escape
        // here avoids the dep on a hot path.
        let before_json = change.before.as_deref().map(json_quote_string);
        let after_json = json_quote_string(&change.after);
        sqlx::query!(
            r#"INSERT INTO mass_edit_history
                   (target_kind, target_id, field,
                    before_value, after_value, batch_id, actor)
                 VALUES (?, ?, ?, ?, ?, ?, ?)"#,
            target_kind,
            target_id,
            field_str,
            before_json,
            after_json,
            batch_id,
            actor,
        )
        .execute(ctx.library.pool())
        .await
        .map_err(|e| Error::Database(format!("mass_edit_history insert: {e}")))?;
    }
    Ok(())
}

/// Minimal JSON string quoter for the `before_value` /
/// `after_value` columns. Escapes `\` / `"` / `\n` / `\r` /
/// `\t` / control bytes — sufficient for the tag strings we
/// write (which are UTF-8 free text). Pulled inline to avoid
/// a `serde_json` dep on this leaf module.
fn json_quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Active `book_files` rows for the book — `(file_id, file_path)`
/// tuples used by the per-file write loop.
async fn load_active_file_paths(
    ctx: &StageContext,
    book_id: BookId,
) -> Result<Vec<(FileId, String)>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        r#"SELECT file_id AS "file_id!: i64", file_path
             FROM book_files
            WHERE book_id = ? AND is_active = 1
            ORDER BY file_id"#,
        id,
    )
    .fetch_all(ctx.library.pool())
    .await
    .map_err(|e| Error::Database(format!("tag-write-early load files: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| (FileId(r.file_id), r.file_path))
        .collect())
}

/// Per-file write: acquire ref, write tags, release ref. The
/// ref guard is explicit because `Drop` can't be async; both
/// success and error paths release before returning.
#[allow(
    clippy::too_many_arguments,
    reason = "ctx + book_id + file_id + path + winners + cover_bytes is the natural minimum for a per-file write; collapsing into a struct would just move the same shape into a wrapper."
)]
async fn write_one_file(
    ctx: &StageContext,
    book_id: BookId,
    file_id: FileId,
    file_path: &str,
    winners: &[FieldWinner],
    cover_bytes: Option<&[u8]>,
) -> Result<WriteReport> {
    let handle = book_file_refs::acquire(
        ctx.library.pool(),
        file_id,
        TAG_WRITE_EARLY_STAGE_NAME,
        book_id,
    )
    .await?;

    // Block on lofty's sync I/O via `spawn_blocking`. Audio
    // file rewrites can run hundreds of ms (tag sections inside
    // large mp4 atoms); blocking the async runtime here would
    // stall every other in-flight stage on the same worker.
    let path_owned = file_path.to_owned();
    let winners_owned: Vec<FieldWinner> = winners.to_vec();
    let cover_owned: Option<Vec<u8>> = cover_bytes.map(<[u8]>::to_vec);
    let result = tokio::task::spawn_blocking(move || {
        write_winners(
            std::path::Path::new(&path_owned),
            &winners_owned,
            cover_owned.as_deref(),
        )
    })
    .await
    .map_err(|e| Error::Io(std::io::Error::other(format!("tag-write-early join: {e}"))))?;

    // Release happens regardless of write outcome — never leak
    // a ref on the error path.
    if let Err(e) = handle.release(ctx.library.pool()).await {
        tracing::warn!(
            book = %book_id,
            file_id = file_id.0,
            error = %e,
            "tag-write.early.release_failed"
        );
    }

    result
}

/// Final-pass tag-write stage (ADR-0028 § `TagWriteFinal`).
///
/// Background priority. Writes every field with a winner that the
/// early pass didn't already write the same value for, EXCEPT
/// fields whose winner has `source = 'user_edit'`. Per ADR-0028 §
/// "Skips per-field on user-edit": the user's correction wins
/// until they explicitly clear it. The skip predicate is
/// [`crate::skip_for_final_pass`].
///
/// ## File targeting
///
/// `load_active_file_paths` already filters on `is_active = 1`,
/// which means post-transcode the m4b row is the only active one
/// (ADR-0027: `PostTranscodeSourcesTarget` flips the source rows
/// to `is_active = 0` once their refs settle). So the late write
/// lands on the surviving file automatically — no explicit
/// "prefer m4b" branch needed.
///
/// Carries its own [`crate::CoverClient`] for the same reason
/// [`TagWriteEarlyStage`] does: a `CoverUrl` winner late-revised
/// by an extractor needs the bytes fetched once per run, shared
/// across every active file.
#[derive(Debug, Clone)]
pub struct TagWriteFinalStage {
    cover: crate::CoverClient,
}

impl TagWriteFinalStage {
    /// Construct from the workspace's HTTP-client tunables.
    /// Mirrors [`TagWriteEarlyStage::from_tunables`]; same
    /// permissive-fallback rationale.
    ///
    /// # Errors
    ///
    /// Surfaces [`crate::CoverFetchError::ClientBuild`] from
    /// the underlying `reqwest::Client::build`.
    pub fn from_tunables(
        tunables: &ab_core::tunables::HttpClientTunables,
    ) -> std::result::Result<Self, crate::CoverFetchError> {
        Ok(Self {
            cover: crate::CoverClient::new(tunables)?,
        })
    }

    /// Construct with the default HTTP-client tunables. Same
    /// permissive-fallback shape as [`TagWriteEarlyStage::new`] —
    /// the daemon must stay bootable even if the cover-art HTTP
    /// client build fails.
    #[must_use]
    pub fn new() -> Self {
        let defaults = ab_core::tunables::HttpClientTunables::default();
        match crate::CoverClient::new(&defaults) {
            Ok(cover) => Self { cover },
            Err(e) => {
                tracing::warn!(error = %e, "tag-write.final.cover_client_build_failed_using_fallback");
                let http = reqwest::Client::new();
                Self {
                    cover: ab_covers::CoverClient::with_parts(http, defaults.cover_max_bytes),
                }
            }
        }
    }
}

impl Default for TagWriteFinalStage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Stage for TagWriteFinalStage {
    fn name(&self) -> &'static str {
        TAG_WRITE_FINAL_STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        TAG_WRITE_FINAL_REQUIRES
    }

    #[allow(
        clippy::too_many_lines,
        reason = "mirrors TagWriteEarlyStage::run by design — same per-book linear flow, with the one ADR-0028 § user-edit filter layered after the winner load. Sharing a helper across the two would force generic comment text that loses each stage's specific rationale."
    )]
    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let winners_all = select_winners_for_book(ctx.library.pool(), book_id.0).await?;
        if winners_all.is_empty() {
            tracing::debug!(
                book = %book_id,
                stage = TAG_WRITE_FINAL_STAGE_NAME,
                "tag-write.final.no_winners"
            );
            return Ok(StageOutcome::Skipped);
        }

        // ADR-0028 § "Skips per-field on user-edit". Strip every
        // winner whose source is `'user_edit'` BEFORE passing
        // them to `write_one_file`; the writer is source-agnostic
        // and will overwrite anything it's handed. Keeping the
        // filter at the stage boundary (rather than inside
        // `write_winners`) preserves Early's existing behavior
        // (Early runs before any user can have edited so the
        // filter would be a no-op there anyway) and keeps the
        // skip predicate documented in one place — see
        // `crate::skip_for_final_pass`.
        let user_edit_skipped: usize = winners_all
            .iter()
            .filter(|w| crate::skip_for_final_pass(&w.source))
            .count();
        let winners: Vec<FieldWinner> = winners_all
            .into_iter()
            .filter(|w| !crate::skip_for_final_pass(&w.source))
            .collect();

        if winners.is_empty() {
            tracing::debug!(
                book = %book_id,
                stage = TAG_WRITE_FINAL_STAGE_NAME,
                user_edit_skipped,
                "tag-write.final.all_winners_user_edit_skip"
            );
            return Ok(StageOutcome::Skipped);
        }

        // Mirror Early's tautology guard: if every surviving
        // winner is itself `tag_file`-sourced, the file already
        // has what we'd write. Skip the I/O cycle. This is the
        // common steady-state case for books that ran through
        // Early without any subsequent AI extractor producing a
        // new winner.
        if winners.iter().all(|w| w.source == "tag_file") {
            tracing::debug!(
                book = %book_id,
                stage = TAG_WRITE_FINAL_STAGE_NAME,
                winner_count = winners.len(),
                user_edit_skipped,
                "tag-write.final.all_winners_from_tag_file_skip"
            );
            return Ok(StageOutcome::Skipped);
        }

        let files = load_active_file_paths(ctx, book_id).await?;
        if files.is_empty() {
            tracing::debug!(
                book = %book_id,
                stage = TAG_WRITE_FINAL_STAGE_NAME,
                "tag-write.final.no_active_files"
            );
            return Ok(StageOutcome::Skipped);
        }

        // Cover-fetch: do it once per run. An extractor that ran
        // between Early and Final may have produced a new
        // CoverUrl winner; fetch the bytes here so the per-file
        // loop can share them. Same best-effort failure handling
        // as Early — log + None on failure; other winners still
        // write.
        let cover_bytes: Option<Vec<u8>> = match winners
            .iter()
            .find(|w| w.field == ab_core::Field::CoverUrl)
            .and_then(|w| w.value.as_deref())
        {
            Some(url) => match self.cover.fetch(url).await {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    tracing::warn!(
                        book = %book_id,
                        url = %url,
                        error = %e,
                        "tag-write.final.cover_fetch_failed"
                    );
                    None
                }
            },
            None => None,
        };

        let batch_id = Uuid::new_v4().to_string();
        let mut total_changes: usize = 0;
        let mut total_matched: usize = 0;
        let mut total_unmapped: usize = 0;
        let mut any_changed = false;
        for (file_id, file_path) in files {
            match write_one_file(
                ctx,
                book_id,
                file_id,
                &file_path,
                &winners,
                cover_bytes.as_deref(),
            )
            .await
            {
                Ok(report) => {
                    total_changes += report.fields_changed();
                    total_matched += report.fields_already_matched;
                    total_unmapped += report.fields_unmapped;
                    if !report.changes.is_empty() {
                        any_changed = true;
                        if let Err(e) =
                            record_audit_rows(ctx, &batch_id, file_id, &report.changes).await
                        {
                            tracing::warn!(
                                book = %book_id,
                                file_id = file_id.0,
                                error = %e,
                                "tag-write.final.audit_log_failed"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        book = %book_id,
                        file_id = file_id.0,
                        path = %file_path,
                        error = %e,
                        "tag-write.final.file_failed"
                    );
                }
            }
        }

        tracing::info!(
            book = %book_id,
            stage = TAG_WRITE_FINAL_STAGE_NAME,
            batch_id = %batch_id,
            fields_changed = total_changes,
            fields_already_matched = total_matched,
            fields_unmapped = total_unmapped,
            user_edit_skipped,
            "tag-write.final.done"
        );

        if any_changed {
            Ok(StageOutcome::Done)
        } else {
            // All surviving winners matched on-disk values; the
            // late re-tag pass is a no-op. Idempotent re-runs of
            // a steady-state book hit this path.
            Ok(StageOutcome::Skipped)
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::{EphemeralDb, LibraryDb};
    use std::path::Path;
    use tempfile::TempDir;

    async fn fresh_ctx(dir: &Path, stage_name: &'static str) -> StageContext {
        let lib = LibraryDb::open(&dir.join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let eph = EphemeralDb::open(&dir.join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        StageContext {
            library: lib,
            ephemeral: eph,
            cancel: tokio_util::sync::CancellationToken::new(),
            stage_name,
        }
    }

    async fn seed_book(ctx: &StageContext, book_id: i64) {
        sqlx::query("INSERT INTO books (book_id, title) VALUES (?, 'fixture')")
            .bind(book_id)
            .execute(ctx.library.pool())
            .await
            .expect("seed book");
    }

    async fn seed_winner(ctx: &StageContext, book_id: i64, field: &str, value: &str, source: &str) {
        sqlx::query(
            "INSERT INTO book_field_provenance \
                 (book_id, field, value, source, stage, confidence, is_winner) \
             VALUES (?, ?, ?, ?, 'test-stage', 0.9, 1)",
        )
        .bind(book_id)
        .bind(field)
        .bind(value)
        .bind(source)
        .execute(ctx.library.pool())
        .await
        .expect("seed provenance");
    }

    #[test]
    fn typed_stage_ids_pin_strings() {
        assert_eq!(TAG_WRITE_EARLY_STAGE_ID.as_str(), "tag-write-early");
        assert_eq!(TAG_WRITE_FINAL_STAGE_ID.as_str(), "tag-write-final");
        assert_eq!(
            TAG_WRITE_EARLY_STAGE_NAME,
            TAG_WRITE_EARLY_STAGE_ID.as_str()
        );
        assert_eq!(
            TAG_WRITE_FINAL_STAGE_NAME,
            TAG_WRITE_FINAL_STAGE_ID.as_str()
        );
    }

    #[tokio::test]
    async fn early_stage_metadata() {
        let s = TagWriteEarlyStage::new();
        assert_eq!(s.name(), "tag-write-early");
        assert_eq!(s.requires(), &[StageId::new("tag-read")]);
    }

    #[tokio::test]
    async fn final_stage_metadata() {
        let s = TagWriteFinalStage::new();
        assert_eq!(s.name(), "tag-write-final");
        // ADR-0028 § "TagWriteFinal `requires()`": all AI
        // extractors + consensus + transcode-m4b + tag-read.
        // Order matters here because the const is laid out that
        // way for readability — the assertion pins both the set
        // and the listing order so a rename surfaces as a diff,
        // not a silent reorder.
        assert_eq!(
            s.requires(),
            &[
                StageId::new("tag-read"),
                StageId::new("promote-consensus"),
                StageId::new("extract-summary-spoiler-free"),
                StageId::new("extract-story-arc"),
                StageId::new("extract-characters"),
                StageId::new("extract-setting"),
                StageId::new("extract-summary-spoiler-free-series"),
                StageId::new("transcode-m4b"),
            ]
        );
    }

    #[tokio::test]
    async fn final_skips_when_no_winners() {
        // No `book_field_provenance` rows for this book → the
        // first guard in `run` short-circuits before any file
        // I/O. Steady-state "this book hasn't progressed past
        // tag-read yet" path.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_FINAL_STAGE_NAME).await;
        let outcome = TagWriteFinalStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn final_skips_when_all_winners_are_user_edits() {
        // ADR-0028 § "Skips per-field on user-edit". When the
        // only winners present have source = 'user_edit', the
        // late pass has nothing left after the filter — it
        // returns Skipped instead of touching the file.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_FINAL_STAGE_NAME).await;
        seed_book(&ctx, 1).await;
        seed_winner(&ctx, 1, "title", "User Title", USER_EDIT_SOURCE).await;
        seed_winner(&ctx, 1, "description", "User Description", USER_EDIT_SOURCE).await;

        let outcome = TagWriteFinalStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn final_skips_when_all_winners_are_tag_file_sourced() {
        // Tautology guard: every remaining winner is already
        // tag_file-sourced, so the on-disk values match by
        // definition. No I/O needed.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_FINAL_STAGE_NAME).await;
        seed_book(&ctx, 1).await;
        seed_winner(&ctx, 1, "title", "Tag File Title", "tag_file").await;
        seed_winner(&ctx, 1, "description", "Tag File Description", "tag_file").await;

        let outcome = TagWriteFinalStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn final_skips_when_no_active_files_present() {
        // Non-tag_file winner survives the filter, but the book
        // has no rows in `book_files` with `is_active = 1` —
        // nothing to write to. Skipped.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_FINAL_STAGE_NAME).await;
        seed_book(&ctx, 1).await;
        seed_winner(
            &ctx,
            1,
            "title",
            "AI Suggested Title",
            "extract-summary-spoiler-free",
        )
        .await;

        let outcome = TagWriteFinalStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn early_skips_when_no_winners() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_EARLY_STAGE_NAME).await;
        seed_book(&ctx, 1).await;

        let outcome = TagWriteEarlyStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped (no winners), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn early_skips_when_only_tag_file_winners() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_EARLY_STAGE_NAME).await;
        seed_book(&ctx, 1).await;
        seed_winner(&ctx, 1, "title", "Foundation", "tag_file").await;
        seed_winner(&ctx, 1, "author", "Asimov", "tag_file").await;

        let outcome = TagWriteEarlyStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped (all tag_file winners), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn early_skips_when_no_active_files() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_EARLY_STAGE_NAME).await;
        seed_book(&ctx, 1).await;
        // Winner from audnexus, NOT tag_file — survives the
        // tautology filter — but no active file rows.
        seed_winner(&ctx, 1, "title", "Foundation", "audnexus-enrich").await;

        let outcome = TagWriteEarlyStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped (no active files), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn early_recovers_when_file_path_is_unreadable() {
        // Pure resilience test: an active row with a path that
        // doesn't exist on disk. lofty fails to open it; the
        // stage logs + continues to the next file (none here),
        // returns Skipped because no successful writes.
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_EARLY_STAGE_NAME).await;
        seed_book(&ctx, 1).await;
        seed_winner(&ctx, 1, "title", "Foundation", "audnexus-enrich").await;
        sqlx::query(
            "INSERT INTO book_files (file_id, book_id, file_path, is_active) \
             VALUES (1, 1, '/nonexistent/path/should/never/exist.mp3', 1)",
        )
        .execute(ctx.library.pool())
        .await
        .expect("seed file");

        let outcome = TagWriteEarlyStage::new()
            .run(&ctx, BookId(1))
            .await
            .expect("run");
        match outcome {
            StageOutcome::Skipped => {}
            other => panic!("expected Skipped (no successful write), got {other:?}"),
        }
    }

    // ── Audit-log helpers (PROJECT.md § Tag-write history) ──

    #[test]
    fn json_quote_escapes_control_and_quote_chars() {
        assert_eq!(json_quote_string("hello"), "\"hello\"");
        assert_eq!(json_quote_string(""), "\"\"");
        assert_eq!(json_quote_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_quote_string("c\\d"), "\"c\\\\d\"");
        assert_eq!(json_quote_string("e\nf"), "\"e\\nf\"");
        assert_eq!(json_quote_string("g\tt"), "\"g\\tt\"");
        // U+0001 (control byte) → 
        assert_eq!(json_quote_string("h\x01i"), "\"h\\u0001i\"");
        // Multibyte UTF-8 is left as-is — strings in this column
        // are valid UTF-8 by construction (lofty returns UTF-8
        // tag values).
        assert_eq!(json_quote_string("Bjørk"), "\"Bjørk\"");
    }

    /// Row shape for the audit-log read-back in
    /// `record_audit_rows_writes_one_row_per_change_with_shared_batch_id`.
    type AuditTuple = (String, i64, String, Option<String>, String, String, String);

    #[tokio::test]
    async fn record_audit_rows_writes_one_row_per_change_with_shared_batch_id() {
        let tmp = TempDir::new().expect("tmpdir");
        let ctx = fresh_ctx(tmp.path(), TAG_WRITE_EARLY_STAGE_NAME).await;
        seed_book(&ctx, 1).await;

        let batch = Uuid::new_v4().to_string();
        let changes = vec![
            FieldChange {
                field: ab_core::Field::Title,
                before: None,
                after: "Foundation".to_owned(),
            },
            FieldChange {
                field: ab_core::Field::Author,
                before: Some("Asimov, I.".to_owned()),
                after: "Isaac Asimov".to_owned(),
            },
        ];

        record_audit_rows(&ctx, &batch, FileId(7), &changes)
            .await
            .expect("audit insert");

        let rows: Vec<AuditTuple> = sqlx::query_as(
            "SELECT target_kind, target_id, field, before_value, after_value, \
             batch_id, actor \
             FROM mass_edit_history \
             ORDER BY edit_id",
        )
        .fetch_all(ctx.library.pool())
        .await
        .expect("read audit");

        assert_eq!(rows.len(), 2, "one row per FieldChange");
        // Row 0: Title, before NULL → before_value column is None
        assert_eq!(rows[0].0, "book_files");
        assert_eq!(rows[0].1, 7);
        assert_eq!(rows[0].2, "title");
        assert_eq!(rows[0].3, None, "absent before-value stays NULL");
        assert_eq!(rows[0].4, "\"Foundation\"");
        assert_eq!(rows[0].5, batch);
        assert_eq!(rows[0].6, "system");
        // Row 1: Author, before captured + escaped.
        assert_eq!(rows[1].2, "author");
        assert_eq!(rows[1].3.as_deref(), Some("\"Asimov, I.\""));
        assert_eq!(rows[1].4, "\"Isaac Asimov\"");
        // Shared batch_id across rows.
        assert_eq!(rows[1].5, batch);
    }
}
