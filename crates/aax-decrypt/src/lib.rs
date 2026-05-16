//! Audible AAX → m4b lossless container swap (ADR-0053 Revision 2).
//!
//! AAX files are MP4 containers carrying encrypted AAC samples
//! (codec tag `aavd`). With the operator's per-account
//! activation-bytes key (8 lowercase hex chars), ffmpeg can
//! decrypt the samples and re-mux them into a standard m4b
//! container with codec tag `mp4a` — no re-encode, identical
//! payload bytes, lossless.
//!
//! This crate is the Rust wrapper around the ffmpeg shell-out.
//! It does NOT own the activation bytes (those resolve via
//! [`ab_core::aax_activation_bytes::resolve`]); the caller hands
//! them in already-validated. The crate's job is the ffmpeg
//! lifecycle + typed errors + stderr-pattern detection for the
//! "wrong activation bytes" failure mode that needs a different
//! operator response than the generic "decrypt failed" path.
//!
//! ## ffmpeg as a runtime dep
//!
//! ADR-0053 Revision 2 inverted the original "ffmpeg dev-only"
//! posture: AVFoundation does not expose an activation-bytes API,
//! so ffmpeg is the production decrypt mechanism. The crate
//! treats missing-on-`PATH` the same way as missing-activation-
//! bytes — `Error::FfmpegNotOnPath` is the typed equivalent, and
//! the upstream pipeline stage maps both to a `Skipped` outcome
//! with an actionable log message.
//!
//! ## Security note (threat-model addendum)
//!
//! Activation bytes pass to ffmpeg via the `-activation_bytes`
//! argv flag. On macOS, `ps` shows the full command line only to
//! the same user (default behaviour without `procmod` entitlement
//! grants). Same-user observability of operator-private credentials
//! is acceptable per the existing trust boundary in ADR-0053 §
//! Activation-bytes storage — the daemon's own memory, environment,
//! and config files already carry the same observability surface.

pub mod stage;

pub use stage::{AaxDecryptStage, STAGE_ID, STAGE_NAME};

use std::path::Path;
use std::process::{Command, Stdio};

use ab_core::aax_activation_bytes::ActivationBytes;

/// Tracing event prefix.
const TRACE_PREFIX: &str = "aax_decrypt";

/// Failure modes for [`decrypt`]. Each variant maps to a
/// distinct operator response, so the caller can split log
/// messages + retry policy by the typed cause.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `ffmpeg` not found on `PATH`. The upstream pipeline stage
    /// maps this to a `Skipped` outcome with a `brew install ffmpeg`
    /// pointer. Identical user-facing handling to
    /// [`Error::ActivationBytesMissing`].
    #[error("ffmpeg not found on PATH (`brew install ffmpeg`)")]
    FfmpegNotOnPath,
    /// ffmpeg reported the activation bytes were wrong (the
    /// stderr line carries `Invalid activation bytes` or
    /// equivalent). Surfaced separately from
    /// [`Error::DecryptFailed`] so the upstream stage can log
    /// the source ASIN (so the operator can correlate against
    /// their purchase history) rather than blaming the file.
    #[error("ffmpeg rejected the activation bytes")]
    ActivationBytesRejected,
    /// Generic ffmpeg exit-nonzero. Wraps the captured stderr
    /// text for the upstream log; the activation bytes are
    /// never reflected back into the error.
    #[error("ffmpeg decrypt failed: {0}")]
    DecryptFailed(String),
    /// ffmpeg exited zero but the output file is missing or
    /// empty. Treated as a separate failure so the upstream
    /// stage doesn't insert a `book_files` row for a bogus
    /// output.
    #[error("ffmpeg succeeded but output is empty: {0}")]
    OutputEmpty(String),
    /// `Command::output` / file-stat I/O error wrapping the
    /// underlying `std::io::Error`.
    #[error("ffmpeg I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Lossless AAX → m4b container swap via ffmpeg shell-out.
///
/// Runs:
///
/// ```text
/// ffmpeg -hide_banner -loglevel error -y \
///     -activation_bytes <bytes_hex> \
///     -i <input> \
///     -c:a copy -map_metadata 0 -map_chapters 0 \
///     <output>
/// ```
///
/// The `-c:a copy` flag preserves the AAC payload verbatim
/// (lossless); `-map_metadata 0` and `-map_chapters 0` carry
/// tags and chapter atoms over from the input. The codec tag
/// flips from `aavd` to `mp4a` in the container as a side
/// effect of the demux / remux cycle.
///
/// # Errors
///
/// See [`Error`] for the typed failure modes.
///
/// # Panics
///
/// Never. All UTF-8 conversions on stderr use `from_utf8_lossy`.
pub fn decrypt(input: &Path, output: &Path, bytes: &ActivationBytes) -> Result<(), Error> {
    tracing::info!(
        target = "{TRACE_PREFIX}",
        input = %input.display(),
        output = %output.display(),
        "{TRACE_PREFIX}.start",
    );

    let result = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-activation_bytes")
        .arg(bytes.as_hex())
        .arg("-i")
        .arg(input)
        .arg("-c:a")
        .arg("copy")
        .arg("-map_metadata")
        .arg("0")
        .arg("-map_chapters")
        .arg("0")
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let result = match result {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                target = "{TRACE_PREFIX}",
                "{TRACE_PREFIX}.ffmpeg_not_on_path"
            );
            return Err(Error::FfmpegNotOnPath);
        }
        Err(e) => return Err(Error::Io(e)),
    };

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
        if classify_stderr(&stderr) == StderrClass::ActivationBytesRejected {
            tracing::warn!(
                target = "{TRACE_PREFIX}",
                "{TRACE_PREFIX}.activation_bytes_rejected"
            );
            return Err(Error::ActivationBytesRejected);
        }
        tracing::warn!(
            target = "{TRACE_PREFIX}",
            stderr = %stderr,
            "{TRACE_PREFIX}.decrypt_failed"
        );
        return Err(Error::DecryptFailed(stderr));
    }

    // ffmpeg exited zero — sanity check the output file. A
    // missing or zero-byte output is a "succeeded but lying"
    // case; treat as a distinct failure.
    let metadata = std::fs::metadata(output)?;
    if metadata.len() == 0 {
        return Err(Error::OutputEmpty(output.display().to_string()));
    }

    tracing::info!(
        target = "{TRACE_PREFIX}",
        bytes_out = metadata.len(),
        "{TRACE_PREFIX}.done"
    );
    Ok(())
}

/// Classification of ffmpeg's stderr output. The `ActivationBytesRejected`
/// case branches on the exact string match against ffmpeg's audible-aac
/// demuxer reject message; all other failures fall into `Generic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StderrClass {
    /// ffmpeg reported the activation bytes were wrong. The
    /// audible-aac demuxer prints `Invalid activation bytes`
    /// (with capital I) when the AES key derived from the bytes
    /// doesn't unlock the AAC samples.
    ActivationBytesRejected,
    /// Anything else.
    Generic,
}

fn classify_stderr(stderr: &str) -> StderrClass {
    // ffmpeg's audible-aac demuxer prints variants of this text
    // when activation_bytes don't match the file's key. Capture
    // the most common phrasings; the production runs will
    // accumulate more cases as we see them in the wild.
    if stderr.contains("Invalid activation bytes")
        || stderr.contains("invalid activation bytes")
        || stderr.contains("activation_bytes is invalid")
    {
        return StderrClass::ActivationBytesRejected;
    }
    StderrClass::Generic
}

/// Check whether `ffmpeg` is callable on `PATH`. Used by the
/// pipeline stage at startup so the daemon can log the missing
/// dependency immediately rather than waiting for the first AAX
/// source to arrive.
///
/// # Errors
///
/// Returns [`Error::FfmpegNotOnPath`] when the spawn fails with
/// `NotFound`, and [`Error::Io`] for any other spawn error.
pub fn check_ffmpeg_on_path() -> Result<(), Error> {
    let result = Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(Error::DecryptFailed(
            "ffmpeg -version returned non-zero status".to_owned(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::FfmpegNotOnPath),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn classify_stderr_detects_invalid_activation_bytes_capital_i() {
        let stderr = "[audible_aac @ 0x123] Invalid activation bytes\nInvalid data found when processing input\n";
        assert_eq!(
            classify_stderr(stderr),
            StderrClass::ActivationBytesRejected
        );
    }

    #[test]
    fn classify_stderr_detects_invalid_activation_bytes_lowercase_i() {
        let stderr = "audible_aac: invalid activation bytes given\n";
        assert_eq!(
            classify_stderr(stderr),
            StderrClass::ActivationBytesRejected
        );
    }

    #[test]
    fn classify_stderr_detects_activation_bytes_is_invalid_phrasing() {
        let stderr = "Error: activation_bytes is invalid for this file.\n";
        assert_eq!(
            classify_stderr(stderr),
            StderrClass::ActivationBytesRejected
        );
    }

    #[test]
    fn classify_stderr_generic_for_unrelated_error() {
        let stderr = "[mov,mp4 @ 0x456] could not find codec parameters\n";
        assert_eq!(classify_stderr(stderr), StderrClass::Generic);
    }

    #[test]
    fn classify_stderr_generic_for_empty_input() {
        assert_eq!(classify_stderr(""), StderrClass::Generic);
    }

    #[test]
    fn classify_stderr_generic_for_disk_full() {
        let stderr = "av_interleaved_write_frame(): No space left on device\n";
        assert_eq!(classify_stderr(stderr), StderrClass::Generic);
    }
}
