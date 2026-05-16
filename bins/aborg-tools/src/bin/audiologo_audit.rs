//! `audiologo-audit` — read-only deep-dive review tool for the
//! audiologo detection cascade (ADR-0054).
//!
//! ## Usage
//!
//! ```bash
//! aborg-tools audiologo-audit \
//!     --corpus ~/dev/Testing/audiobooks \
//!     --out ./audit-report \
//!     [--limit N]
//! ```
//!
//! Walks the corpus, extracts 60-second clips from the start +
//! end of each audio file, renders SVG waveforms, and emits an
//! HTML report with an embedded rating UI ("good / improve /
//! bad" + comment per book). The operator listens, rates,
//! exports a JSON annotation file that informs the next
//! ADR-0024 R5 revision pass.
//!
//! Phase 1 (this binary): no detection pipeline wiring. Clips
//! land at the **start + end** of the file (not at proposed
//! cut offsets), and the report's "Detection method" badge
//! reads "Phase 1 — detection wiring pending." Useful for the
//! operator's initial "is there a jingle?" pass; full
//! detection-method capture lands in a Phase 2 PR that wires
//! the `DetectAudiologoStage` against an ephemeral DB.
//!
//! ffmpeg is required (clip extraction). Binary errors out
//! cleanly if missing, pointing at `brew install ffmpeg`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use aborg_tools::audit::{
    AuditEntry, DetectionInfo,
    clips::{self, CLIP_DURATION_SECS},
    report,
    walk::{self, SourceFile},
    waveform,
};

#[derive(Parser)]
#[command(about = "Audiologo audit — read-only corpus review (ADR-0054)")]
struct Args {
    /// Corpus directory to walk (recursively).
    #[arg(long)]
    corpus: PathBuf,
    /// Output directory for the HTML report + clips + waveforms.
    #[arg(long)]
    out: PathBuf,
    /// Cap on number of books to audit. Useful for first runs
    /// against a large library.
    #[arg(long)]
    limit: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    if !args.corpus.is_dir() {
        anyhow::bail!("corpus path {} is not a directory", args.corpus.display());
    }

    clips::ensure_ffmpeg_present().context("ffmpeg is required for clip extraction")?;

    tracing::info!(corpus = %args.corpus.display(), "audiologo_audit.walk.start");
    let sources = walk::walk_corpus(&args.corpus, args.limit)?;
    tracing::info!(count = sources.len(), "audiologo_audit.walk.done");

    let clips_dir = args.out.join("clips");
    std::fs::create_dir_all(&clips_dir)
        .with_context(|| format!("mkdir {}", clips_dir.display()))?;

    let mut entries = Vec::with_capacity(sources.len());
    let mut used_slugs = std::collections::HashSet::new();
    for (idx, src) in sources.iter().enumerate() {
        match build_entry(idx, src, &clips_dir, &mut used_slugs) {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {
                // AAX skip — currently silent; the future Phase 2
                // wiring will emit a "needs aax-decrypt" stub
                // entry so the report shows them too.
                tracing::info!(
                    file = %src.path.display(),
                    "audiologo_audit.skip_aax"
                );
            }
            Err(e) => {
                tracing::warn!(
                    file = %src.path.display(),
                    error = %e,
                    "audiologo_audit.entry_failed"
                );
            }
        }
    }

    report::write_report(&args.out, &args.corpus, &entries)?;
    tracing::info!(
        out = %args.out.display(),
        count = entries.len(),
        "audiologo_audit.report.written"
    );
    Ok(())
}

/// Build one `AuditEntry`. Returns `Ok(None)` for AAX sources
/// (skipped until ADR-0053 lands the decrypt stage).
fn build_entry(
    idx: usize,
    src: &SourceFile,
    clips_dir: &Path,
    used_slugs: &mut std::collections::HashSet<String>,
) -> Result<Option<AuditEntry>> {
    if src.extension == "aax" {
        return Ok(None);
    }

    let mut slug = walk::slugify(&src.title);
    // Disambiguate slug collisions (different books with the
    // same title — happens with multi-edition releases).
    let original = slug.clone();
    let mut suffix = 2;
    while !used_slugs.insert(slug.clone()) {
        slug = format!("{original}-{suffix}");
        suffix += 1;
        if suffix > 99 {
            slug = format!("{original}-{idx}");
            break;
        }
    }

    // Phase 1: front clip = first CLIP_DURATION_SECS of the
    // file; end clip = last CLIP_DURATION_SECS. When Phase 2
    // wires detection, clips will center on the proposed cut
    // offsets instead.
    let front_clip_name = format!("{slug}-front.m4a");
    let end_clip_name = format!("{slug}-end.m4a");
    let front_clip_path = clips_dir.join(&front_clip_name);
    let end_clip_path = clips_dir.join(&end_clip_name);

    clips::extract_clip(&src.path, &front_clip_path, 0, CLIP_DURATION_SECS)
        .with_context(|| format!("front-clip extract for {}", src.path.display()))?;

    let end_start_ms = src
        .duration_ms
        .saturating_sub(u64::from(CLIP_DURATION_SECS) * 1000);
    clips::extract_clip(&src.path, &end_clip_path, end_start_ms, CLIP_DURATION_SECS)
        .with_context(|| format!("end-clip extract for {}", src.path.display()))?;

    let front_waveform_svg =
        waveform::render(&front_clip_path, None).unwrap_or_else(|_| empty_waveform_inline());
    let end_waveform_svg =
        waveform::render(&end_clip_path, None).unwrap_or_else(|_| empty_waveform_inline());

    Ok(Some(AuditEntry {
        slug,
        title: src.title.clone(),
        source_path: src.path.clone(),
        duration_ms: src.duration_ms,
        publisher: src.publisher.clone(),
        copyright: src.copyright.clone(),
        detection: DetectionInfo::Stub,
        front_clip_rel: format!("clips/{front_clip_name}"),
        end_clip_rel: format!("clips/{end_clip_name}"),
        front_waveform_svg,
        end_waveform_svg,
    }))
}

fn empty_waveform_inline() -> String {
    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 200" width="100%"><rect width="1200" height="200" fill="#f5f7fa"/><text x="20" y="100" font-family="monospace" font-size="14" fill="#94a3b8">waveform render failed</text></svg>"##.to_owned()
}
