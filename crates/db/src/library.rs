//! Canonical library database. Holds everything that survives a crash
//! and represents user-facing data.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Pool, Sqlite};

use ab_core::tunables::DbTunables;
use ab_core::{Error, Result};

/// Handle to the library database connection pool. Cheap to clone.
#[derive(Clone)]
pub struct LibraryDb {
    pool: Pool<Sqlite>,
}

impl LibraryDb {
    /// Open or create the library DB at `path`, run migrations,
    /// return a pooled handle.
    ///
    /// Pool sizing + busy-timeout come from `tunables` (defaults in
    /// `ab_core::tunables::DbTunables`).
    pub async fn open(path: &Path, tunables: &DbTunables) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_millis(tunables.busy_timeout_ms));

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(tunables.library_pool_max)
            .connect_with(options)
            .await
            .map_err(|e| Error::Database(format!("open library db: {e}")))?;

        super::migrations::run_library(&pool).await?;

        Ok(Self { pool })
    }

    /// Underlying pool (for crates that build their own queries).
    pub const fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }
}
