//! `aax-decrypt` pipeline stage (ADR-0053 S10.4).

#![allow(
    clippy::doc_markdown,
    clippy::too_long_first_doc_paragraph,
    clippy::too_many_arguments,
    clippy::manual_let_else
)]

//! Runs the [`crate::decrypt`] helper across every active AAX
//! source for a book. Produces an m4b `book_files` row per
//! decrypted source with the same shape `transcode-m4b` emits,
//! so the existing `PostTranscodeSourcesTarget` cleanup target
//! reaps the AAX source after the AI consumers finish reading.
//!
//! ## Source filter
//!
//! `format = 'aax' AND is_active = 1`. AAX rows with codec_tag
//! `aavd` are the only candidates; the scan stage tags them
//! correctly when it sees `.aax` extensions, and the
//! `aborg aax info` helper (PR #127) is the canonical way to
//! verify the tag.
//!
//! ## Activation-bytes resolution
//!
//! Resolved once per stage invocation via
//! [`ab_core::aax_activation_bytes::resolve`]. Missing bytes →
//! stage returns [`StageOutcome::Skipped`] with an
//! `aax_decrypt.activation_bytes.missing` log line. The
//! `aborg doctor aax` line + `aborg aax set-bytes` CLI exist
//! specifically to surface + remediate this case.
//!
//! ## Path-jail integration
//!
//! Each source's path is canonicalized + longest-prefix-matched
//! against the active `library_roots` table + jailed via
//! [`ab_core::trust_zones::TrustZoneJail<LibraryRoot>`] (ADR-0049
//! step 3). The output path is derived inside the same jail by
//! swapping the source's extension to `.m4b`. Same shape as
//! `ab_transcode::output_resolve`; a future slice can lift the
//! shared logic to `ab-core` or `ab-db` when a third consumer
//! arrives.
//!
//! ## Ref-counting
//!
//! Same shape as `transcode-m4b`: acquire a
//! [`ab_db::book_file_refs`] ref before any work; release after
//! the decrypt completes so a long m4b insert can't accidentally
//! hold the source alive past the decrypt itself.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use ab_core::aax_activation_bytes;
use ab_core::trust_zones::{LibraryRoot, TrustZoneJail};
use ab_core::tunables::AudioTunables;
use ab_core::{BookId, FileId, Result};
use ab_db::book_file_refs;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use crate::{Error as DecryptError, check_ffmpeg_on_path, decrypt};

/// Typed stage identifier.
pub const STAGE_ID: StageId = StageId::new("aax-decrypt");

/// Convenience alias for use as the `book_file_refs` purpose.
pub const STAGE_NAME: &str = STAGE_ID.as_str();

/// AAX decrypt stage. Carries an [`AudioTunables`] snapshot for
/// the config-leg activation-bytes lookup; the env-var + Keychain
/// legs are read fresh on every resolve call (no daemon-restart
/// needed to pick up Keychain edits).
#[derive(Debug, Clone)]
pub struct AaxDecryptStage {
    audio: AudioTunables,
}

impl AaxDecryptStage {
    /// Construct with the daemon's loaded audio tunables.
    #[must_use]
    pub const fn new(audio: AudioTunables) -> Self {
        Self { audio }
    }
}

#[async_trait]
impl Stage for AaxDecryptStage {
    fn name(&self) -> &'static str {
        STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        // Same posture as transcode-m4b — runs in parallel with
        // every other stage. The output m4b row + the source AAX
        // row coexist on `is_active = 1` until cleanup reaps the
        // source (per ADR-0027's transcode lifecycle, which the
        // PostTranscodeSourcesTarget shares with us).
        &[]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let book_id_raw = book_id.0;

        // Idempotency gate: if this book already has an active
        // m4b row, the stage's work is done — either we ran
        // previously, or transcode-m4b raced ahead, or the book
        // arrived with an m4b alongside its AAX. Same gate as
        // transcode-m4b.
        let already_m4b: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64"
                 FROM book_files
                WHERE book_id = ? AND format = 'm4b' AND is_active = 1"#,
            book_id_raw,
        )
        .fetch_one(ctx.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("aax_decrypt m4b-exists check: {e}")))?;
        if already_m4b > 0 {
            tracing::debug!(book = %book_id, "aax_decrypt.skip_already_m4b");
            return Ok(StageOutcome::Skipped);
        }

        let sources = sqlx::query!(
            r#"SELECT file_id  AS "file_id!: i64",
                      file_path AS "file_path!: String"
                 FROM book_files
                WHERE book_id = ?
                  AND is_active = 1
                  AND format = 'aax'
                ORDER BY file_id"#,
            book_id_raw,
        )
        .fetch_all(ctx.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("aax_decrypt sources query: {e}")))?;

        if sources.is_empty() {
            tracing::debug!(book = %book_id, "aax_decrypt.skip_no_aax_sources");
            return Ok(StageOutcome::Skipped);
        }

        // Resolve activation bytes once per book. Missing →
        // stage returns Skipped; the doctor line + `aborg aax
        // set-bytes` CLI are the remediation path.
        let Some((bytes, source)) = aax_activation_bytes::resolve(&self.audio) else {
            tracing::warn!(
                book = %book_id,
                source_count = sources.len(),
                "aax_decrypt.activation_bytes.missing"
            );
            return Ok(StageOutcome::Skipped);
        };
        tracing::info!(
            book = %book_id,
            source = %source.tag(),
            source_count = sources.len(),
            "aax_decrypt.start"
        );

        // ffmpeg-on-PATH probe. Missing → same skip posture as
        // missing activation bytes; the operator's response is
        // `brew install ffmpeg`.
        if matches!(check_ffmpeg_on_path(), Err(DecryptError::FfmpegNotOnPath)) {
            tracing::warn!(book = %book_id, "aax_decrypt.ffmpeg.not_on_path");
            return Ok(StageOutcome::Skipped);
        }

        // Library roots, canonicalized once per book.
        let library_roots = load_library_roots(ctx.library.pool())
            .await
            .map_err(|e| ab_core::Error::Database(format!("aax_decrypt library_roots: {e}")))?;

        let mut any_succeeded = false;
        for src in sources {
            if ctx.cancel.is_cancelled() {
                tracing::info!(book = %book_id, "aax_decrypt.cancelled");
                break;
            }

            if process_one_source(
                ctx,
                book_id,
                src.file_id,
                &src.file_path,
                &library_roots,
                &bytes,
            )
            .await?
            {
                any_succeeded = true;
            }
        }

        if any_succeeded {
            Ok(StageOutcome::Done)
        } else {
            Ok(StageOutcome::Skipped)
        }
    }

    /// Reset: delete every active m4b row this stage might have
    /// produced (same predicate as transcode-m4b's reset since
    /// both stages produce the same row shape). The cleanup
    /// target stops reaping the source after this, so the
    /// stage's next pass re-derives the m4b from the original
    /// AAX.
    async fn reset(&self, ctx: &StageContext, book_id: BookId) -> Result<()> {
        let book_id_raw = book_id.0;

        let rows = sqlx::query!(
            r#"SELECT file_id AS "file_id!: i64",
                      file_path AS "file_path!: String"
                 FROM book_files
                WHERE book_id = ? AND format = 'm4b' AND is_active = 1"#,
            book_id_raw,
        )
        .fetch_all(ctx.library.pool())
        .await
        .map_err(|e| ab_core::Error::Database(format!("aax_decrypt reset query: {e}")))?;

        for row in rows {
            // Best-effort on-disk delete; the row goes either way.
            let _ = tokio::fs::remove_file(&row.file_path).await;
            sqlx::query!("DELETE FROM book_files WHERE file_id = ?", row.file_id)
                .execute(ctx.library.pool())
                .await
                .map_err(|e| ab_core::Error::Database(format!("aax_decrypt reset delete: {e}")))?;
        }

        Ok(())
    }
}

/// Process one AAX source: trust-zone-jail validation,
/// `book_file_refs` lifecycle, ffmpeg decrypt, m4b row insert.
/// Returns `Ok(true)` on success, `Ok(false)` on a skip /
/// per-source failure (logged but non-fatal to the stage),
/// `Err` on a DB-level failure.
async fn process_one_source(
    ctx: &StageContext,
    book_id: BookId,
    file_id_raw: i64,
    file_path_raw: &str,
    library_roots: &[PathBuf],
    bytes: &aax_activation_bytes::ActivationBytes,
) -> Result<bool> {
    let book_id_raw = book_id.0;
    let file_id = FileId(file_id_raw);
    let input_path = PathBuf::from(file_path_raw);

    let Some((input, output)) = jailed_input_output(&input_path, library_roots).await else {
        return Ok(false);
    };

    let handle = book_file_refs::acquire(ctx.library.pool(), file_id, STAGE_NAME, book_id).await?;

    let result = decrypt(input.as_path(), output.as_path(), bytes);

    handle.release(ctx.library.pool()).await?;

    if let Err(e) = result {
        match e {
            DecryptError::ActivationBytesRejected => {
                // Don't blame the file — surface the asin (via
                // file_id since asin isn't on book_files) so
                // the operator can correlate against their
                // purchase history.
                tracing::warn!(
                    book = %book_id,
                    file_id = file_id_raw,
                    source = %input.as_path().display(),
                    "aax_decrypt.activation_bytes.rejected"
                );
            }
            other => {
                tracing::warn!(
                    book = %book_id,
                    file_id = file_id_raw,
                    source = %input.as_path().display(),
                    error = %other,
                    "aax_decrypt.file_failed"
                );
            }
        }
        return Ok(false);
    }

    // Stat the output for size; insert the m4b row.
    let size: Option<i64> = match tokio::fs::metadata(output.as_path()).await {
        Ok(m) => i64::try_from(m.len()).ok(),
        Err(e) => {
            tracing::warn!(
                book = %book_id,
                file_id = file_id_raw,
                output = %output.as_path().display(),
                error = %e,
                "aax_decrypt.output_stat_failed"
            );
            None
        }
    };
    let output_str = output.as_path().to_string_lossy().into_owned();
    sqlx::query!(
        "INSERT INTO book_files \
           (book_id, file_path, file_size, format, is_active) \
         VALUES (?, ?, ?, 'm4b', 1)",
        book_id_raw,
        output_str,
        size,
    )
    .execute(ctx.library.pool())
    .await
    .map_err(|e| ab_core::Error::Database(format!("aax_decrypt insert m4b row: {e}")))?;

    tracing::info!(
        book = %book_id,
        src_file_id = file_id_raw,
        source = %input.as_path().display(),
        output = %output.as_path().display(),
        "aax_decrypt.file_completed"
    );
    Ok(true)
}

/// Path-jail input + output. Same shape as
/// `ab_transcode::output_resolve::resolve_for_source` but
/// inlined to avoid a cross-stage dep edge; lift to a shared
/// helper once cover-resize / library-reorg add a third
/// consumer.
async fn jailed_input_output(
    source: &Path,
    library_roots: &[PathBuf],
) -> Option<(
    ab_core::trust_zones::TrustedPath<LibraryRoot>,
    ab_core::trust_zones::TrustedPath<LibraryRoot>,
)> {
    let canonical = match tokio::fs::canonicalize(source).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                source = %source.display(),
                error = %e,
                "aax_decrypt.canonicalize_failed"
            );
            return None;
        }
    };

    let root = library_roots
        .iter()
        .filter(|r| canonical.starts_with(r))
        .max_by_key(|r| r.as_os_str().len())
        .map(PathBuf::as_path)?;

    let jail = match TrustZoneJail::<LibraryRoot>::new(root) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(error = %e, "aax_decrypt.jail_new_failed");
            return None;
        }
    };

    let relative_input = canonical.strip_prefix(root).ok()?;
    let input = jail.join(relative_input).ok()?;
    let relative_output = relative_input.with_extension("m4b");
    let output = jail.join(&relative_output).ok()?;

    Some((input, output))
}

/// Load active library_roots, canonicalize each, drop
/// unreachable ones. Same shape as
/// `ab_transcode::output_resolve::library_roots_canonical`.
async fn load_library_roots(pool: &sqlx::SqlitePool) -> Result<Vec<PathBuf>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT path AS "path!: String"
             FROM library_roots
            WHERE is_active = 1"#,
    )
    .fetch_all(pool)
    .await?;

    let mut roots = Vec::with_capacity(rows.len());
    for r in rows {
        match tokio::fs::canonicalize(&r.path).await {
            Ok(canonical) => roots.push(canonical),
            Err(e) => tracing::warn!(
                path = %r.path,
                error = %e,
                "aax_decrypt.library_root_unreachable"
            ),
        }
    }
    Ok(roots)
}
