//! Emit the current SQL schema (as derived from migrations) to a
//! file. Used by `xtask check` to detect drift.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// Output path for the schema dump.
    #[arg(long, default_value = "docs/SCHEMA.sql")]
    output: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _args = Args::parse();
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    tracing::warn!("schema_snapshot not yet implemented in scaffold");
    Ok(())
}
