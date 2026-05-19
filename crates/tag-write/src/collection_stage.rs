//! `write-tags-collection` pipeline stage.
//!
//! Reads a book's collection memberships from
//! `book_collection_members` JOIN `book_collections`, then writes
//! the resulting (name, kind) pair to every active `book_files`
//! row via [`crate::collection::write_collection_pair`].
//!
//! ## Source-of-truth posture (tag-as-truth)
//!
//! Collections are derived in the DB by the scanner box-set
//! heuristic + manual operator edits via
//! `POST/PATCH /api/v1/collections`. Embedding the membership
//! in the file's tags closes the loop: the audio file becomes
//! self-describing — anyone reading it later with mp3tag /
//! `MusicBrainz` Picard / a future ABorganizer rescan sees the
//! collection without having to consult the DB.
//!
//! ## Single-collection scope (slice 1)
//!
//! A book in multiple collections is uncommon (typically box-set
//! XOR compilation XOR curated, with at most one curated overlay).
//! Slice 1 picks the earliest membership by `added_at` and emits
//! a `tracing::warn` if a book belongs to more than one. Multi-
//! collection encoding (repeated TXXX frames on ID3v2.4 / multiple
//! freeform atoms on MP4) is a follow-up slice once the single-
//! pair shape is exercised in real catalogues.
//!
//! ## Stage ordering
//!
//! `requires: [read-tags]`. The stage reads through the active
//! `book_files` rows, so post-transcode it writes only to the
//! surviving m4b (matching the `TagWriteFinalStage` file-targeting
//! pattern). It does NOT require `transcode-m4b` — books in
//! collections may legitimately remain in their source format.

use async_trait::async_trait;

use ab_core::{BookId, Error, FileId, Result};
use ab_db::book_file_refs;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

use crate::collection::{CollectionPairOutcome, write_collection_pair};

/// Typed stage identifier.
pub const WRITE_TAGS_COLLECTION_STAGE_ID: StageId = StageId::new("write-tags-collection");
/// Stable `&'static str` mirror of [`WRITE_TAGS_COLLECTION_STAGE_ID`].
pub const WRITE_TAGS_COLLECTION_STAGE_NAME: &str = WRITE_TAGS_COLLECTION_STAGE_ID.as_str();

/// `Stage::requires` set. Only `read-tags` — the membership data
/// is in the DB, not derived from any other Stage's output. Active-
/// file filtering handles the post-transcode case implicitly.
const REQUIRES: &[StageId] = &[StageId::new("read-tags")];

/// Stage impl. Stateless; cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct WriteTagsCollectionStage;

impl WriteTagsCollectionStage {
    /// Construct. No tunables yet — slice 1 ships always-on.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Stage for WriteTagsCollectionStage {
    fn name(&self) -> &'static str {
        WRITE_TAGS_COLLECTION_STAGE_NAME
    }

    fn requires(&self) -> &'static [StageId] {
        REQUIRES
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let Some(membership) = load_primary_collection(ctx, book_id).await? else {
            tracing::debug!(
                book = %book_id,
                stage = WRITE_TAGS_COLLECTION_STAGE_NAME,
                "tag-write.collection.no_membership"
            );
            return Ok(StageOutcome::Skipped);
        };

        let files = load_active_file_paths(ctx, book_id).await?;
        if files.is_empty() {
            tracing::debug!(
                book = %book_id,
                stage = WRITE_TAGS_COLLECTION_STAGE_NAME,
                "tag-write.collection.no_active_files"
            );
            return Ok(StageOutcome::Skipped);
        }

        let mut any_changed = false;
        let mut matched: usize = 0;
        let mut unmapped: usize = 0;
        for (file_id, file_path) in files {
            match write_one_file(ctx, book_id, file_id, &file_path, &membership).await {
                Ok(CollectionPairOutcome::Changed { .. }) => any_changed = true,
                Ok(CollectionPairOutcome::Matched) => matched += 1,
                Ok(CollectionPairOutcome::Unmapped) => unmapped += 1,
                Err(e) => {
                    tracing::warn!(
                        book = %book_id,
                        file_id = file_id.0,
                        path = %file_path,
                        error = %e,
                        "tag-write.collection.file_failed"
                    );
                }
            }
        }

        tracing::info!(
            book = %book_id,
            stage = WRITE_TAGS_COLLECTION_STAGE_NAME,
            collection_name = %membership.name,
            collection_kind = %membership.kind,
            any_changed,
            matched,
            unmapped,
            "tag-write.collection.done"
        );

        if any_changed {
            Ok(StageOutcome::Done)
        } else {
            Ok(StageOutcome::Skipped)
        }
    }
}

/// One book's primary collection membership — the earliest by
/// `added_at` when there are multiple.
struct PrimaryCollection {
    name: String,
    kind: String,
}

async fn load_primary_collection(
    ctx: &StageContext,
    book_id: BookId,
) -> Result<Option<PrimaryCollection>> {
    let id = book_id.0;
    let rows = sqlx::query!(
        r#"SELECT c.name AS "name!: String",
                  c.kind AS "kind!: String",
                  m.added_at AS "added_at!: i64"
             FROM book_collection_members m
             JOIN book_collections c ON c.collection_id = m.collection_id
            WHERE m.book_id = ?
            ORDER BY m.added_at ASC, m.member_id ASC"#,
        id,
    )
    .fetch_all(ctx.library.pool())
    .await
    .map_err(|e| Error::Database(format!("write-tags-collection load: {e}")))?;

    if rows.is_empty() {
        return Ok(None);
    }

    if rows.len() > 1 {
        let other_names: Vec<String> = rows.iter().skip(1).map(|r| r.name.clone()).collect();
        tracing::warn!(
            book = %book_id,
            primary = %rows[0].name,
            additional = ?other_names,
            "tag-write.collection.multi_membership_truncated"
        );
    }

    Ok(Some(PrimaryCollection {
        name: rows[0].name.clone(),
        kind: rows[0].kind.clone(),
    }))
}

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
    .map_err(|e| Error::Database(format!("write-tags-collection load files: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| (FileId(r.file_id), r.file_path))
        .collect())
}

async fn write_one_file(
    ctx: &StageContext,
    book_id: BookId,
    file_id: FileId,
    file_path: &str,
    membership: &PrimaryCollection,
) -> Result<CollectionPairOutcome> {
    let handle = book_file_refs::acquire(
        ctx.library.pool(),
        file_id,
        WRITE_TAGS_COLLECTION_STAGE_NAME,
        book_id,
    )
    .await?;

    let path_owned = file_path.to_owned();
    let name_owned = membership.name.clone();
    let kind_owned = membership.kind.clone();
    let result = tokio::task::spawn_blocking(move || {
        write_collection_pair(std::path::Path::new(&path_owned), &name_owned, &kind_owned)
    })
    .await
    .map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "write-tags-collection join: {e}"
        )))
    })?;

    if let Err(e) = handle.release(ctx.library.pool()).await {
        tracing::warn!(
            book = %book_id,
            file_id = file_id.0,
            error = %e,
            "tag-write.collection.release_failed"
        );
    }

    result
}
