//! Migration runners. Each migration is a numbered `.sql` file in
//! `migrations/{library,ephemeral}/`. `sqlx::migrate!` discovers and
//! applies them at startup.

use sqlx::{Pool, Sqlite};

use ab_core::{Error, Result};

/// Apply every pending migration to the library DB.
pub async fn run_library(pool: &Pool<Sqlite>) -> Result<()> {
    sqlx::migrate!("./migrations/library")
        .run(pool)
        .await
        .map_err(|e| Error::Database(format!("library migrate: {e}")))?;
    tracing::info!("db.library.migrated");
    Ok(())
}

/// Apply every pending migration to the ephemeral DB.
pub async fn run_ephemeral(pool: &Pool<Sqlite>) -> Result<()> {
    sqlx::migrate!("./migrations/ephemeral")
        .run(pool)
        .await
        .map_err(|e| Error::Database(format!("ephemeral migrate: {e}")))?;
    tracing::info!("db.ephemeral.migrated");
    Ok(())
}
