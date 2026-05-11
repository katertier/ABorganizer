//! Fast-iteration bench for whole-book fingerprinting.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// Audio file to fingerprint.
    file: PathBuf,
    /// Approximate total duration in seconds (probe will replace this
    /// when wired).
    #[arg(long, default_value_t = 3600)]
    duration_secs: u32,
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let windows = ab_fingerprint::fingerprint_file(&args.file, args.duration_secs)?;
    tracing::info!(
        file = ?args.file,
        windows = windows.len(),
        "fingerprint_bench.done"
    );
    Ok(())
}
