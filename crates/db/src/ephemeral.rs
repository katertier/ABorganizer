//! Ephemeral database for the job queue, pipeline progress, rate-limit
//! state, and other restartable state. Survives crashes (it's on disk)
//! but does not need to be backed up — recoverable from scratch.

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Pool, Sqlite};

use ab_core::{Error, Result};

/// Handle to the ephemeral database connection pool. Cheap to clone.
#[derive(Clone)]
pub struct EphemeralDb {
    pool: Pool<Sqlite>,
}

impl EphemeralDb {
    /// Open or create the ephemeral DB at `path`, run migrations,
    /// return a pooled handle.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // OFF is acceptable here: data is restartable by design.
            .synchronous(SqliteSynchronous::Off)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(|e| Error::Database(format!("open ephemeral db: {e}")))?;

        super::migrations::run_ephemeral(&pool).await?;

        Ok(Self { pool })
    }

    /// Underlying pool (for crates that build their own queries).
    pub const fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }
}
