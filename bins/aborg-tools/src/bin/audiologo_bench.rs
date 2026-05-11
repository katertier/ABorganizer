//! Fast-iteration bench for audiologo detection.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// Audio file to analyse.
    file: PathBuf,
    /// Detect intro or outro.
    #[arg(long, value_enum, default_value = "intro")]
    kind: Kind,
}

#[derive(Clone, clap::ValueEnum)]
enum Kind {
    Intro,
    Outro,
}

impl From<Kind> for ab_audiologo::Kind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Intro => Self::Intro,
            Kind::Outro => Self::Outro,
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let result = ab_audiologo::detect(&args.file, args.kind.into())?;
    tracing::info!(file = ?args.file, ?result, "audiologo_bench.done");
    Ok(())
}
