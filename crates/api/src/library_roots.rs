//! `library_roots` REST surface (backlog item 3).
//!
//! Three endpoints:
//!
//! - `GET    /api/v1/library_roots` — list every active root.
//! - `POST   /api/v1/library_roots` — register a new root.
//! - `DELETE /api/v1/library_roots/{root_id}` — soft-delete
//!   (`is_active = 0`).
//!
//! Hard-delete is intentionally not exposed — the audit trail
//! (`created_at`, original path) survives for forensics. The
//! `aborg clean db` umbrella will eventually offer a hard-delete
//! sweep for very old soft-deleted roots.
//!
//! # Why this exists
//!
//! Previously `tunables.security.library_roots` was the only
//! way to register a scannable path — config.toml edit + daemon
//! restart. That works for the dev box but breaks the
//! API-first / CLI-and-GUI-are-clients rule
//! (see `[api-first-cli-vs-gui-split]` user memory). With these
//! endpoints in place every surface (CLI `aborg roots`, future
//! web UI, voice control) manages roots through one path.
//!
//! **B.7 closure note:** the one-cycle bridge that seeded this
//! table from the deprecated `tunables.security.library_roots`
//! Vec is removed (tracker #119). Operators register roots via
//! the REST surface; a stale `library_roots = [...]` setting in
//! `config.toml` is now rejected as an `unknown_field` by
//! `SecurityTunables`.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use ab_core::Error;

use crate::error::ApiError;
use crate::state::ApiState;

/// One row from the `library_roots` table, serialised over HTTP.
/// Mirrors the schema exactly except `is_active` (we filter to
/// active in `GET` and use the field only on the server side).
#[derive(Debug, Clone, Serialize)]
pub struct LibraryRoot {
    /// Auto-increment primary key from `library_roots.root_id`.
    pub root_id: i64,
    /// Canonical absolute path. Always exists on disk at
    /// registration time (the POST handler canonicalises);
    /// re-canonicalisation on each scan catches paths that
    /// later become unavailable (NAS unmounted etc.).
    pub path: String,
    /// Operator-friendly label. `None` if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Unix seconds since epoch.
    pub created_at: i64,
}

/// Request body for `POST /api/v1/library_roots`.
#[derive(Debug, Deserialize)]
pub struct CreateLibraryRootRequest {
    /// Path to register. The handler canonicalises before
    /// storage so symlinks resolve and trailing-slash variants
    /// don't create duplicates.
    pub path: String,
    /// Optional operator-friendly label.
    #[serde(default)]
    pub label: Option<String>,
}

/// `GET /api/v1/library_roots` — every active root, oldest first.
///
/// # Errors
///
/// Bubbles DB failures as `ApiError` (500-class). No 4xx paths
/// — listing is read-only and doesn't validate inputs.
pub async fn library_roots_list(
    State(state): State<ApiState>,
) -> Result<Json<Vec<LibraryRoot>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT root_id AS "root_id!: i64",
                  path    AS "path!: String",
                  label   AS "label?: String",
                  created_at AS "created_at!: i64"
             FROM library_roots
            WHERE is_active = 1
            ORDER BY created_at ASC, root_id ASC"#,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| Error::Database(format!("library_roots list: {e}")))?;
    let out: Vec<LibraryRoot> = rows
        .into_iter()
        .map(|r| LibraryRoot {
            root_id: r.root_id,
            path: r.path,
            label: r.label,
            created_at: r.created_at,
        })
        .collect();
    Ok(Json(out))
}

/// `POST /api/v1/library_roots` — register a new root.
///
/// Steps:
/// 1. Canonicalise `req.path` (rejects non-existent paths).
/// 2. Verify it's a directory (we don't scan files directly).
/// 3. Reject if the same canonical path already exists as an
///    active row.
/// 4. INSERT (or revive a soft-deleted row if one matches).
///
/// On collision with an **active** row → 409 Conflict with the
/// existing `root_id` so the caller can dedupe. On collision
/// with a **soft-deleted** row → revive it (flip `is_active`
/// back to 1, return 200) so re-adding a previously-removed
/// root is idempotent.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] — path doesn't canonicalise, isn't
///   a directory, or is the empty string.
/// - [`ApiError::Conflict`] — a different active row already
///   carries this canonical path.
pub async fn library_roots_create(
    State(state): State<ApiState>,
    Json(req): Json<CreateLibraryRootRequest>,
) -> Result<(StatusCode, Json<LibraryRoot>), ApiError> {
    let canonical = canonicalise_directory(&req.path)?;
    let canonical_str = canonical
        .to_str()
        .ok_or_else(|| {
            ApiError::BadRequest(format!("path is not valid UTF-8: {}", canonical.display()))
        })?
        .to_owned();
    let label = req.label.as_deref().map(str::trim).and_then(|s| {
        if s.is_empty() {
            None
        } else {
            Some(s.to_owned())
        }
    });

    // Check for an existing row first (active OR soft-deleted)
    // so we can dedupe before the INSERT — easier to reason about
    // than `INSERT ON CONFLICT DO UPDATE` against a UNIQUE that
    // ignores the active flag.
    let existing = sqlx::query!(
        r#"SELECT root_id AS "root_id!: i64",
                  is_active AS "is_active!: i64",
                  created_at AS "created_at!: i64"
             FROM library_roots
            WHERE path = ?"#,
        canonical_str,
    )
    .fetch_optional(state.inner.library.pool())
    .await
    .map_err(|e| Error::Database(format!("library_roots create lookup: {e}")))?;
    if let Some(row) = existing {
        if row.is_active == 1 {
            return Err(ApiError::Conflict(format!(
                "library root {canonical_str} already exists (root_id={})",
                row.root_id
            )));
        }
        // Revive the soft-deleted row. We also refresh the label
        // — the caller's POST is the new source of truth.
        sqlx::query!(
            "UPDATE library_roots
                SET is_active = 1,
                    label = ?
              WHERE root_id = ?",
            label,
            row.root_id,
        )
        .execute(state.inner.library.pool())
        .await
        .map_err(|e| Error::Database(format!("library_roots revive: {e}")))?;
        tracing::info!(
            root_id = row.root_id,
            path = %canonical_str,
            "api.library_roots.revived"
        );
        return Ok((
            StatusCode::OK,
            Json(LibraryRoot {
                root_id: row.root_id,
                path: canonical_str,
                label,
                created_at: row.created_at,
            }),
        ));
    }

    let now = unix_now_secs();
    let inserted = sqlx::query!(
        "INSERT INTO library_roots (path, label, created_at)
         VALUES (?, ?, ?)
         RETURNING root_id AS \"root_id!: i64\"",
        canonical_str,
        label,
        now,
    )
    .fetch_one(state.inner.library.pool())
    .await
    .map_err(|e| Error::Database(format!("library_roots insert: {e}")))?;
    tracing::info!(
        root_id = inserted.root_id,
        path = %canonical_str,
        "api.library_roots.created"
    );
    Ok((
        StatusCode::CREATED,
        Json(LibraryRoot {
            root_id: inserted.root_id,
            path: canonical_str,
            label,
            created_at: now,
        }),
    ))
}

/// `DELETE /api/v1/library_roots/{root_id}` — soft-delete one
/// root (sets `is_active = 0`). Idempotent: a second DELETE on
/// the same row is a no-op `204`.
///
/// # Errors
///
/// - [`ApiError::NotFound`] when no row matches `root_id`.
pub async fn library_roots_delete(
    State(state): State<ApiState>,
    AxumPath(root_id): AxumPath<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let affected = sqlx::query!(
        "UPDATE library_roots
            SET is_active = 0
          WHERE root_id = ? AND is_active = 1",
        root_id,
    )
    .execute(state.inner.library.pool())
    .await
    .map_err(|e| Error::Database(format!("library_roots delete: {e}")))?
    .rows_affected();
    if affected == 0 {
        // Distinguish "row doesn't exist" from "already inactive"
        // — the former is 404, the latter is 204 (idempotent).
        let exists: Option<i64> = sqlx::query_scalar!(
            "SELECT root_id FROM library_roots WHERE root_id = ?",
            root_id,
        )
        .fetch_optional(state.inner.library.pool())
        .await
        .map_err(|e| Error::Database(format!("library_roots delete check: {e}")))?;
        if exists.is_none() {
            return Err(ApiError::NotFound(format!("library_root {root_id}")));
        }
    }
    tracing::info!(
        root_id,
        rows_affected = affected,
        "api.library_roots.deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Canonicalise + validate `requested` is an existing directory.
/// Used by `create` and by daemon startup's seed-from-tunables
/// path so the canonicalisation rules don't diverge.
///
/// # Errors
///
/// [`ApiError::BadRequest`] when the path is empty, doesn't
/// exist, or doesn't resolve to a directory.
pub fn canonicalise_directory(requested: &str) -> Result<PathBuf, ApiError> {
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        return Err(ApiError::BadRequest("path must not be empty".to_owned()));
    }
    let canonical = std::fs::canonicalize(Path::new(trimmed)).map_err(|e| {
        ApiError::BadRequest(format!(
            "path does not exist or is not readable ({trimmed}): {e}"
        ))
    })?;
    let meta = std::fs::metadata(&canonical).map_err(|e| {
        ApiError::BadRequest(format!(
            "cannot stat canonical path {}: {e}",
            canonical.display()
        ))
    })?;
    if !meta.is_dir() {
        return Err(ApiError::BadRequest(format!(
            "path is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// True when `requested` (already canonicalised) is at-or-under
/// any active row in `library_roots`. Replaces the old
/// `validate_scan_path` against the tunable vector.
///
/// Each stored root is re-canonicalised at check time so a NAS
/// that's been remounted at a different path is rejected
/// instead of silently passing.
///
/// # Errors
///
/// Database / IO errors bubble as `ApiError`. The handler's
/// caller surfaces an empty-list case as 400 ("no roots
/// configured") on its own — this helper returns `Ok(false)`.
pub async fn path_is_under_any_root(
    state: &ApiState,
    canonical_requested: &Path,
) -> Result<bool, ApiError> {
    let roots = sqlx::query!(
        r#"SELECT path AS "path!: String"
             FROM library_roots
            WHERE is_active = 1"#,
    )
    .fetch_all(state.inner.library.pool())
    .await
    .map_err(|e| Error::Database(format!("library_roots path check: {e}")))?;
    for r in roots {
        if let Ok(canonical_root) = std::fs::canonicalize(&r.path)
            && canonical_requested.starts_with(&canonical_root)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn unix_now_secs() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use ab_core::tunables::{DbTunables, SchedulerTunables, SecurityTunables};
    use ab_db::{EphemeralDb, LibraryDb};
    use ab_pipeline::cleanup::CleanupRegistry;
    use ab_pipeline::{Dag, Scheduler, StageContext};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    async fn fresh_state() -> (ApiState, TempDir) {
        let tmp = TempDir::new().expect("tmpdir");
        let library = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        let ephemeral = EphemeralDb::open(&tmp.path().join("ephemeral.db"), &DbTunables::default())
            .await
            .expect("open ephemeral");
        let cancel = CancellationToken::new();
        let dag = Arc::new(Dag::build(Vec::new()).expect("empty dag builds"));
        let stage_ctx = StageContext {
            library: library.clone(),
            ephemeral: ephemeral.clone(),
            cancel: cancel.clone(),
            stage_name: "test",
        };
        let scheduler = Arc::new(Scheduler::spawn(
            Arc::clone(&dag),
            stage_ctx,
            &SchedulerTunables::default(),
        ));
        let cleanup = CleanupRegistry::new(Vec::new());
        let state = ApiState::new(
            library,
            ephemeral,
            scheduler,
            dag,
            cleanup,
            cancel,
            SecurityTunables::default(),
            globset::GlobSet::empty(),
            ab_background::BackgroundRegistry::new(vec![]),
            crate::doctor::DoctorRegistry::new(vec![]),
        );
        (state, tmp)
    }

    #[tokio::test]
    async fn list_is_empty_on_fresh_db() {
        let (state, _tmp) = fresh_state().await;
        let Json(rows) = library_roots_list(State(state)).await.expect("list");
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn create_then_list_returns_row() {
        let (state, tmp) = fresh_state().await;
        let scan_root = tmp.path().join("Audiobooks");
        std::fs::create_dir(&scan_root).expect("mkdir");

        let req = CreateLibraryRootRequest {
            path: scan_root.to_string_lossy().to_string(),
            label: Some("Local SSD".to_owned()),
        };
        let (status, Json(row)) = library_roots_create(State(state.clone()), Json(req))
            .await
            .expect("create");
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(row.label.as_deref(), Some("Local SSD"));
        assert!(row.path.ends_with("Audiobooks"));

        let Json(rows) = library_roots_list(State(state)).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].root_id, row.root_id);
    }

    #[tokio::test]
    async fn create_rejects_nonexistent_path() {
        let (state, _tmp) = fresh_state().await;
        let req = CreateLibraryRootRequest {
            path: "/definitely/does/not/exist/123".to_owned(),
            label: None,
        };
        let r = library_roots_create(State(state), Json(req)).await;
        assert!(matches!(r, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn create_rejects_file_not_directory() {
        let (state, tmp) = fresh_state().await;
        let file = tmp.path().join("not-a-dir.txt");
        std::fs::write(&file, b"x").expect("write");

        let req = CreateLibraryRootRequest {
            path: file.to_string_lossy().to_string(),
            label: None,
        };
        let r = library_roots_create(State(state), Json(req)).await;
        assert!(matches!(r, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn create_rejects_empty_path() {
        let (state, _tmp) = fresh_state().await;
        let req = CreateLibraryRootRequest {
            path: "   ".to_owned(),
            label: None,
        };
        let r = library_roots_create(State(state), Json(req)).await;
        assert!(matches!(r, Err(ApiError::BadRequest(_))));
    }

    #[tokio::test]
    async fn create_returns_conflict_on_duplicate_active() {
        let (state, tmp) = fresh_state().await;
        let dir = tmp.path().join("A");
        std::fs::create_dir(&dir).expect("mkdir");
        let s = dir.to_string_lossy().to_string();

        let r1 = library_roots_create(
            State(state.clone()),
            Json(CreateLibraryRootRequest {
                path: s.clone(),
                label: None,
            }),
        )
        .await
        .expect("first");
        assert_eq!(r1.0, StatusCode::CREATED);

        let r2 = library_roots_create(
            State(state),
            Json(CreateLibraryRootRequest {
                path: s,
                label: None,
            }),
        )
        .await;
        assert!(matches!(r2, Err(ApiError::Conflict(_))), "got {r2:?}");
    }

    #[tokio::test]
    async fn delete_then_recreate_revives_existing_row() {
        let (state, tmp) = fresh_state().await;
        let dir = tmp.path().join("B");
        std::fs::create_dir(&dir).expect("mkdir");
        let s = dir.to_string_lossy().to_string();

        let (_, Json(row)) = library_roots_create(
            State(state.clone()),
            Json(CreateLibraryRootRequest {
                path: s.clone(),
                label: Some("Original".to_owned()),
            }),
        )
        .await
        .expect("create");
        let root_id = row.root_id;

        let _ = library_roots_delete(State(state.clone()), AxumPath(root_id))
            .await
            .expect("delete");

        let (status, Json(revived)) = library_roots_create(
            State(state.clone()),
            Json(CreateLibraryRootRequest {
                path: s.clone(),
                label: Some("Renamed".to_owned()),
            }),
        )
        .await
        .expect("recreate");
        assert_eq!(status, StatusCode::OK, "revive returns 200, not 201");
        assert_eq!(
            revived.root_id, root_id,
            "same row id — soft-deleted row was revived"
        );
        assert_eq!(revived.label.as_deref(), Some("Renamed"));

        let Json(rows) = library_roots_list(State(state)).await.expect("list");
        assert_eq!(rows.len(), 1, "no duplicate row created");
    }

    #[tokio::test]
    async fn delete_unknown_id_is_not_found() {
        let (state, _tmp) = fresh_state().await;
        let r = library_roots_delete(State(state), AxumPath(99_999)).await;
        assert!(matches!(r, Err(ApiError::NotFound(_))));
    }

    #[tokio::test]
    async fn delete_already_inactive_is_idempotent_204() {
        let (state, tmp) = fresh_state().await;
        let dir = tmp.path().join("C");
        std::fs::create_dir(&dir).expect("mkdir");
        let s = dir.to_string_lossy().to_string();

        let (_, Json(row)) = library_roots_create(
            State(state.clone()),
            Json(CreateLibraryRootRequest {
                path: s,
                label: None,
            }),
        )
        .await
        .expect("create");
        let _ = library_roots_delete(State(state.clone()), AxumPath(row.root_id))
            .await
            .expect("first delete");
        // Second delete on the now-inactive row should return
        // 204 (idempotent), NOT 404 — the row still exists.
        let r = library_roots_delete(State(state), AxumPath(row.root_id)).await;
        assert!(r.is_ok(), "second delete must be a no-op success");
    }

    #[tokio::test]
    async fn path_is_under_any_root_matches_subdir() {
        let (state, tmp) = fresh_state().await;
        let root = tmp.path().join("Lib");
        std::fs::create_dir_all(root.join("Sub")).expect("mkdir");

        // Seed via the create handler so canonicalisation goes
        // through the same path as production.
        let (_, Json(_row)) = library_roots_create(
            State(state.clone()),
            Json(CreateLibraryRootRequest {
                path: root.to_string_lossy().to_string(),
                label: None,
            }),
        )
        .await
        .expect("create");

        let sub = std::fs::canonicalize(root.join("Sub")).expect("canonicalize sub");
        let under = path_is_under_any_root(&state, &sub).await.expect("check");
        assert!(under, "subdir must be under registered root");

        // A sibling outside the root must not match.
        let outside = tmp.path().join("Other");
        std::fs::create_dir(&outside).expect("mkdir");
        let outside_canon = std::fs::canonicalize(&outside).expect("canonicalize outside");
        let under_other = path_is_under_any_root(&state, &outside_canon)
            .await
            .expect("check2");
        assert!(!under_other, "sibling dir must NOT match");
    }

    // `seed_from_tunables_inserts_valid_and_skips_invalid` removed
    // with the seed bridge in slice B.7 (tracker #119).
}
