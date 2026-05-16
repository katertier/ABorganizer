//! 60-second audio clip extraction via ffmpeg shell-out
//! (ADR-0054).

#![allow(clippy::cast_precision_loss)]
//!
//! ffmpeg is a dev-only dep for this binary. We chose ffmpeg
//! over AVFoundation FFI because:
//!
//! * ffmpeg's `-ss <start> -t <duration>` accepts millisecond
//!   precision and produces a clean stream-copied AAC output
//!   from m4b / m4a / mp3 sources without re-encode.
//! * The audit binary is a one-shot dev tool; adding a new
//!   Swift FFI surface just for clip extraction would be
//!   gratuitous when the operator already has ffmpeg installed.
//! * If ffmpeg is missing the binary errors out cleanly with
//!   a pointer to `brew install ffmpeg`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Default clip duration in seconds (audit-visible window
/// per bookend).
pub const CLIP_DURATION_SECS: u32 = 60;

/// Lazy `ffmpeg` discovery — runs once per binary invocation.
///
/// # Errors
///
/// Returns an error if `ffmpeg --version` doesn't return
/// status 0, pointing the operator at `brew install ffmpeg`.
pub fn ensure_ffmpeg_present() -> Result<()> {
    let out = Command::new("ffmpeg").arg("-version").output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => bail!(
            "ffmpeg --version returned non-zero status ({}). \
             Install via `brew install ffmpeg`.",
            o.status
        ),
        Err(e) => bail!(
            "ffmpeg not found ({e}). Install via `brew install ffmpeg`. \
             Audit clip extraction requires ffmpeg as a dev-only dep."
        ),
    }
}

/// Extract a `duration_secs` clip from `input` starting at
/// `start_ms`. Output format is `.m4a` (AAC-in-MP4) for
/// HTML5 `<audio>` compatibility.
///
/// Stream-copy (`-c copy`) when the source is already
/// AAC-in-MP4; ffmpeg falls back to re-encode otherwise.
/// We accept ffmpeg's own choice — the audit doesn't need
/// fine-grained codec control.
///
/// `start_ms` is clamped to be non-negative; the caller is
/// responsible for keeping `start_ms + duration_secs * 1000`
/// within file bounds (ffmpeg silently truncates anyway, so
/// the audit reports just show shorter clips at file
/// boundaries).
///
/// # Errors
///
/// Returns an error on ffmpeg invocation failure or non-zero
/// exit status. Stderr from ffmpeg surfaces in the error
/// message for diagnostic.
pub fn extract_clip(input: &Path, output: &Path, start_ms: u64, duration_secs: u32) -> Result<()> {
    let start_secs = (start_ms as f64) / 1000.0;
    let start_str = format!("{start_secs:.3}");
    let dur_str = duration_secs.to_string();

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create clip parent {}", parent.display()))?;
    }

    let out = Command::new("ffmpeg")
        .arg("-y") // overwrite output without prompting
        .arg("-loglevel")
        .arg("error")
        // -ss before -i for input-side seek (faster); ffmpeg's
        // modern build is keyframe-accurate enough for audio
        // clips. Re-checks with `-accurate_seek` would slow it
        // down without measurable benefit for a 60s window.
        .arg("-ss")
        .arg(&start_str)
        .arg("-t")
        .arg(&dur_str)
        .arg("-i")
        .arg(input)
        // Stream-copy if codec is AAC, else fall back to AAC
        // 96k (still inside reasonable size). `-c:a aac
        // -b:a 96k` is a reliable default; the upstream
        // stream-copy path is best-effort with `-c copy`. We
        // pick re-encode here unconditionally to avoid the
        // bookkeeping of two ffmpeg invocations — clips are
        // ~600 KB each at 96k, not load-bearing.
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("96k")
        .arg("-vn") // no video stream
        .arg(output)
        .output()
        .with_context(|| format!("invoke ffmpeg for {}", input.display()))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "ffmpeg failed extracting clip from {}: status={}, stderr={}",
            input.display(),
            out.status,
            stderr.trim()
        );
    }

    Ok(())
}
