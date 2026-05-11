//! Fast-iteration bench for whole-book fingerprinting.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// Audio file to fingerprint.
    file: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let windows = ab_fingerprint::fingerprint_file(&args.file)?;
    tracing::info!(
        file = ?args.file,
        windows = windows.len(),
        "fingerprint_bench.done"
    );
    for w in &windows {
        tracing::info!(
            offset_sec = w.offset_sec,
            duration_sec = w.duration_sec,
            fp_len = w.fingerprint.len(),
            "fingerprint_bench.window"
        );
    }
    Ok(())
}
