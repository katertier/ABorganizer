//! Lightweight audio file probes. No FFI — pure Rust via lofty.

use std::path::Path;

use ab_core::Result;
use lofty::file::AudioFile;

/// Best-effort total duration in milliseconds.
///
/// Probes the file via [`lofty::read_from_path`] and reads
/// [`lofty::file::FileProperties::duration`]. Behaviour:
///
/// - **Format recognised, duration available** → `Ok(Some(ms))`.
/// - **Format not recognised by lofty** (parse failure, unknown
///   container, corrupt header) → `Ok(None)`. The probe is
///   best-effort; callers treat this as "duration unknown" and
///   fall back to other sources (catalog metadata, scan-time
///   filesystem stat, etc.).
/// - **I/O failure** (file missing, permission denied, disk
///   error) → `Err(ab_core::Error::Io)`. Distinguishing this
///   from a parse failure lets callers retry on transient I/O
///   without re-attempting on a broken file.
///
/// `u64::try_from(duration.as_millis())` is fallible in theory
/// (`Duration` carries `u128` millis ≈ 292 million years); a
/// real audio file overflowing `u64` ms is a non-finding worth
/// treating as `None` rather than an error.
///
/// # Errors
///
/// Returns [`ab_core::Error::Io`] when the file can't be opened
/// at the filesystem layer. `lofty`'s format-parsing errors do
/// NOT surface here — they collapse into `Ok(None)`.
pub fn probe_duration_ms(file: &Path) -> Result<Option<u64>> {
    // Validate I/O at the filesystem level FIRST. lofty's error
    // type does carry an `Io` variant, but it co-mingles parse
    // failures and disk failures inside one `LoftyError::Io`.
    // Pre-checking via `metadata()` cleanly separates the two
    // concerns without parsing lofty's error internals: if the
    // file can't even be stat'd, that's `Error::Io`; from there,
    // any lofty failure is necessarily a format / parse concern.
    let _ = std::fs::metadata(file)?;

    let Ok(tagged) = lofty::read_from_path(file) else {
        // Parse failure — best-effort returns None per the doc.
        // No log here: callers (read-tags stage, audiologo probe)
        // already log the "couldn't probe" decision at their own
        // semantic level.
        return Ok(None);
    };

    let duration = tagged.properties().duration();
    let ms = u64::try_from(duration.as_millis()).ok();
    Ok(ms)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, reason = "test setup idioms")]

    use super::probe_duration_ms;
    use std::path::Path;

    #[test]
    fn missing_file_returns_io_error() {
        // Probing a path that doesn't exist surfaces as
        // `ab_core::Error::Io`, NOT `Ok(None)`. The distinction
        // matters because callers might retry transient I/O but
        // shouldn't retry a permanently-broken file.
        let result =
            probe_duration_ms(Path::new("/tmp/definitely-not-a-real-audio-file-aborg.m4b"));
        assert!(
            matches!(result, Err(ab_core::Error::Io(_))),
            "missing file should yield Error::Io, got {result:?}"
        );
    }

    #[test]
    fn unrecognised_format_returns_none() {
        // A real file that ISN'T audio (plain text) trips
        // lofty's parser; the function returns `Ok(None)` per
        // the "format isn't recognised" contract.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"this is not audio").expect("write");
        let result = probe_duration_ms(tmp.path()).expect("io ok");
        assert_eq!(
            result, None,
            "non-audio file should yield Ok(None) (format not recognised)"
        );
    }

    /// Generate a 0.5-second silent audio fixture at `path` via
    /// ffmpeg. Returns `None` if ffmpeg isn't on PATH so the
    /// calling test silent-skips. Same shape as the tag-write
    /// crate's fixture helper — kept in-crate rather than shared
    /// to avoid coupling test helpers across crates.
    fn ffmpeg_silence(path: &Path, codec: &str) -> Option<()> {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=mono",
                "-t",
                "0.5",
                "-c:a",
                codec,
                "-b:a",
                "64k",
            ])
            .arg(path)
            .status()
            .ok()?;
        status.success().then_some(())
    }

    /// Real round-trip: a 0.5-second silent MP3 should report
    /// roughly 500ms via the probe. The exact value depends on
    /// the encoder's frame alignment (libmp3lame typically
    /// reports 522ms for a 500ms input due to MP3 frame
    /// granularity), so we assert a band rather than an exact
    /// value.
    ///
    /// Silent-skip when ffmpeg isn't on PATH.
    #[test]
    fn mp3_duration_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("silence.mp3");
        if ffmpeg_silence(&path, "libmp3lame").is_none() {
            return;
        }

        let result = probe_duration_ms(&path).expect("probe ok");
        let ms = result.expect("duration available for valid MP3");
        assert!(
            (400..=700).contains(&ms),
            "expected ~500ms (band 400..=700 for encoder frame alignment), got {ms}ms"
        );
    }

    /// Real round-trip: a 0.5-second silent M4A. AAC encoders
    /// align differently than MP3 (typically much closer to the
    /// nominal value); same band-assertion shape catches drift.
    #[test]
    fn m4a_duration_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("silence.m4a");
        if ffmpeg_silence(&path, "aac").is_none() {
            return;
        }

        let result = probe_duration_ms(&path).expect("probe ok");
        let ms = result.expect("duration available for valid M4A");
        assert!(
            (400..=700).contains(&ms),
            "expected ~500ms (band 400..=700 for encoder frame alignment), got {ms}ms"
        );
    }
}
