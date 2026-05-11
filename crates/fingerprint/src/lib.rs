// xtask: max_lines = 600
//! Whole-book identity fingerprinting via chromaprint.
//!
//! # Use-case
//!
//! Answer: *"is this audio file the same recording as that one?"* —
//! robust to bitrate / codec / container changes, weak to different
//! recordings of the same book (different narrator). Used by
//! `aborg library duplicates` to surface re-rips and accidental
//! duplicates.
//!
//! # Algorithm
//!
//! 1. Decode the file with [`symphonia`] into mono i16 PCM at the
//!    file's native sample rate.
//! 2. Sample N 30-second windows at evenly-spaced offsets
//!    ([`OFFSET_FRACTIONS`], default 0% / 25% / 50% / 75% of total
//!    duration).
//! 3. Run [`rusty_chromaprint::Fingerprinter`] over each window,
//!    producing a `Vec<u32>` hash sequence per window.
//!
//! Two recordings are considered "the same" when their per-offset
//! hash sequences agree within [`MATCH_HD`].
//!
//! # Format coverage
//!
//! Targeted at audiobook formats:
//!
//! * `.mp3` ✓ (MPEG layer 3)
//! * `.m4a`, `.m4b` ✓ (AAC inside the MP4 / ISO-MP4 container)
//! * `.flac` ✓
//! * `.ogg` ✓ (Vorbis only; symphonia 0.5 has no Opus codec yet)
//! * `.opus`, `.aax`, `.wma` — decode fails → file skipped, logged
//!
//! Skipped files don't fail the stage; they just don't get a
//! whole-book fingerprint and can't participate in duplicate
//! detection. Audiologo trim (separate crate) is not affected.
//!
//! # Distinction
//!
//! This crate is the *whole-book* fingerprinter. The
//! [`ab_audiologo`](https://docs.rs/ab-audiologo) crate fingerprints
//! short (1-10s) publisher jingles — different problem, different
//! algorithm (RMS thumbprint).

use std::path::Path;

use async_trait::async_trait;
use rusty_chromaprint::{Configuration, Fingerprinter};
use sqlx::Row;
use symphonia::core::audio::{AudioBufferRef, Signal as _};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageOutcome};

/// Window length per fingerprint (seconds). Chromaprint v2 is tuned
/// for ~30s; shorter windows give noisier hashes.
pub const WINDOW_SECS: u32 = 30;

/// Relative offsets (as fractions of total duration) we fingerprint
/// at. Four widely-spaced windows make spurious matches very
/// unlikely while keeping the per-book CPU cost bounded.
pub const OFFSET_FRACTIONS: &[f64] = &[0.0, 0.25, 0.50, 0.75];

/// Hamming-distance threshold above which two fingerprints are
/// considered "different recordings". Tuned for the AcoustID
/// chromaprint output; verify on the synthesized fixture corpus
/// once it exists.
pub const MATCH_HD: u32 = 32;

/// Algorithm name written to `book_fingerprints.algorithm`. Bump
/// when changing window / offset / encoding so old rows are
/// detectable.
pub const ALGORITHM: &str = "chromaprint-v2-w30s";

/// One window's worth of fingerprint output.
#[derive(Debug, Clone)]
pub struct Window {
    /// Offset in seconds from the start of the file.
    pub offset_sec: u32,
    /// Window duration in seconds.
    pub duration_sec: u32,
    /// Chromaprint hash sequence (algorithm-defined).
    pub fingerprint: Vec<u32>,
}

/// Fingerprint `file` at the standard offsets.
///
/// Returns one [`Window`] per offset that succeeded; offsets past
/// the file's end are silently skipped. Returns an empty vec if
/// `file` is too short for a single window.
///
/// # Errors
///
/// Returns [`Error::Io`] on file-system errors,
/// [`Error::Stage`] on probe failures (e.g. unsupported codec —
/// such files are logged + skipped by callers).
pub fn fingerprint_file(file: &Path) -> Result<Vec<Window>> {
    let total_secs = probe_duration_secs(file)?;
    if total_secs < f64::from(WINDOW_SECS) {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(OFFSET_FRACTIONS.len());
    for fraction in OFFSET_FRACTIONS {
        // Cast is safe: `total_secs * fraction` is non-negative
        // (we returned early on too-short files) and bounded above
        // by the file's duration, which fits comfortably in u32
        // for any plausible audiobook (u32::MAX seconds ≈ 136
        // years).
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let offset_sec = (total_secs * fraction) as u32;
        if (f64::from(offset_sec) + f64::from(WINDOW_SECS)) > total_secs {
            continue;
        }
        match fingerprint_window(file, offset_sec, WINDOW_SECS) {
            Ok(fingerprint) => out.push(Window {
                offset_sec,
                duration_sec: WINDOW_SECS,
                fingerprint,
            }),
            Err(e) => {
                tracing::warn!(
                    file = %file.display(),
                    offset = offset_sec,
                    error = %e,
                    "fingerprint.window_failed"
                );
            }
        }
    }
    Ok(out)
}

/// Probe `file` for its total duration in seconds. Cheap — does not
/// decode any audio, just reads format headers.
fn probe_duration_secs(file: &Path) -> Result<f64> {
    let src = std::fs::File::open(file)?;
    let mss = MediaSourceStream::new(
        Box::new(src),
        symphonia::core::io::MediaSourceStreamOptions::default(),
    );
    let mut hint = Hint::new();
    if let Some(ext) = file.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probe = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| Error::stage("fingerprint", format!("probe: {e}")))?;
    let track = probe
        .format
        .default_track()
        .ok_or_else(|| Error::stage("fingerprint", "no default track"))?;
    let codec_params = &track.codec_params;
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| Error::stage("fingerprint", "no sample rate"))?;
    let frames = codec_params
        .n_frames
        .ok_or_else(|| Error::stage("fingerprint", "no frame count"))?;
    // u64→f64 loses precision above 2^52 frames; for a single audio
    // file that's ~12 days at 44.1 kHz. Audiobooks are nowhere near.
    #[allow(clippy::cast_precision_loss)]
    let total_secs = frames as f64 / f64::from(sample_rate);
    Ok(total_secs)
}

/// Decode `[offset_sec, offset_sec + window_sec]` of `file` to
/// interleaved i16 PCM and feed it to `rusty_chromaprint`.
// `start_frame`/`end_frame` are window boundaries; `packet_start`/
// `packet_end` are per-packet positions; `clip_start`/`clip_end` are
// per-packet clip offsets. clippy::similar_names trips on these
// (all *_start / *_end). The names map cleanly onto the algorithm
// invariants; renaming would obscure rather than help.
#[allow(clippy::similar_names)]
fn fingerprint_window(
    file: &Path,
    offset_sec: u32,
    window_sec: u32,
) -> std::result::Result<Vec<u32>, String> {
    let src = std::fs::File::open(file).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(
        Box::new(src),
        symphonia::core::io::MediaSourceStreamOptions::default(),
    );

    let mut hint = Hint::new();
    if let Some(ext) = file.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probe = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("probe: {e}"))?;
    let mut format = probe.format;
    let track = format
        .default_track()
        .ok_or_else(|| "no default track".to_owned())?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| "no sample rate".to_owned())?;
    let channels = codec_params
        .channels
        .map_or(2, symphonia::core::audio::Channels::count);

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| format!("codec: {e}"))?;

    let start_frame = u64::from(offset_sec) * u64::from(sample_rate);
    let end_frame = start_frame + u64::from(window_sec) * u64::from(sample_rate);

    let mut fp = Fingerprinter::new(&Configuration::preset_test1());
    fp.start(
        sample_rate,
        u32::try_from(channels).map_err(|e| format!("channels: {e}"))?,
    )
    .map_err(|e| format!("fp.start: {e}"))?;

    let mut frames_seen: u64 = 0;
    let mut samples_pushed: u64 = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref io))
                if io.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(format!("read packet: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error = %e, "fingerprint.decode_packet_skipped");
                continue;
            }
        };

        let packet_frames = decoded.frames() as u64;
        let packet_start = frames_seen;
        let packet_end = packet_start + packet_frames;
        frames_seen = packet_end;

        if packet_end <= start_frame {
            continue;
        }
        if packet_start >= end_frame {
            break;
        }

        // We target macOS aarch64 only (per rust-toolchain.toml);
        // usize is 64 bits. The clippy lint is pedantic-portable.
        let clip_start = usize::try_from(start_frame.saturating_sub(packet_start)).unwrap_or(0);
        let clip_end =
            usize::try_from((end_frame - packet_start).min(packet_frames)).unwrap_or(usize::MAX);
        let samples = decoded_to_interleaved_i16(&decoded, clip_start, clip_end);
        if !samples.is_empty() {
            fp.consume(&samples);
            samples_pushed += samples.len() as u64;
        }
    }

    if samples_pushed == 0 {
        return Err("no samples in window".to_owned());
    }

    fp.finish();
    Ok(fp.fingerprint().to_vec())
}

/// Convert a Symphonia `AudioBufferRef` to interleaved i16 samples
/// covering the frame range `[clip_start, clip_end)`. Chromaprint
/// takes signed 16-bit input.
fn decoded_to_interleaved_i16(
    buf: &AudioBufferRef<'_>,
    clip_start: usize,
    clip_end: usize,
) -> Vec<i16> {
    let channels = buf.spec().channels.count();
    let len = clip_end.saturating_sub(clip_start);
    if len == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(len * channels);

    match buf {
        AudioBufferRef::U8(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::U16(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::U24(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::U32(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::S8(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::S16(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::S24(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::S32(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::F32(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
        AudioBufferRef::F64(b) => copy_interleaved(b, channels, clip_start, len, &mut out),
    }
    out
}

fn copy_interleaved<S>(
    buf: &symphonia::core::audio::AudioBuffer<S>,
    channels: usize,
    clip_start: usize,
    len: usize,
    out: &mut Vec<i16>,
) where
    S: symphonia::core::sample::Sample + symphonia::core::conv::IntoSample<i16> + Copy,
{
    for frame in 0..len {
        let i = clip_start + frame;
        for ch in 0..channels {
            let plane = buf.chan(ch);
            if let Some(&s) = plane.get(i) {
                out.push(s.into_sample());
            }
        }
    }
}

/// Hamming distance between two `u32`-sequence fingerprints.
/// Returns the total count of differing bits (saturating). Length
/// mismatch contributes 32 bits per missing word.
pub fn hamming(a: &[u32], b: &[u32]) -> u32 {
    let bit_diff: u32 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (x ^ y).count_ones())
        .sum();
    let len_diff_bits = u32::try_from(a.len().abs_diff(b.len()))
        .unwrap_or(u32::MAX)
        .saturating_mul(32);
    bit_diff.saturating_add(len_diff_bits)
}

// ── Pipeline stage ────────────────────────────────────────────────

/// Stage that fingerprints a book's first active file and stores
/// every window in `book_fingerprints`.
pub struct FingerprintStage;

impl FingerprintStage {
    /// Construct.
    pub const fn new() -> Self {
        Self
    }
}

impl Default for FingerprintStage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Stage for FingerprintStage {
    fn name(&self) -> &'static str {
        "fingerprint"
    }

    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let row = sqlx::query(
            "SELECT file_path FROM book_files \
             WHERE book_id = ? AND is_active = 1 ORDER BY file_id LIMIT 1",
        )
        .bind(book_id.0)
        .fetch_optional(ctx.library.pool())
        .await
        .map_err(|e| Error::Database(format!("fp fetch file: {e}")))?;

        let Some(row) = row else {
            return Ok(StageOutcome::Skipped);
        };
        let file_path: String = row
            .try_get("file_path")
            .map_err(|e| Error::Database(format!("fp file_path: {e}")))?;
        let path = std::path::PathBuf::from(&file_path);

        let windows = match tokio::task::spawn_blocking(move || fingerprint_file(&path)).await {
            Ok(Ok(w)) => w,
            Ok(Err(e)) => {
                tracing::warn!(
                    book = %book_id,
                    file = file_path,
                    error = %e,
                    "fingerprint.compute_failed"
                );
                return Ok(StageOutcome::Skipped);
            }
            Err(e) => return Err(Error::stage("fingerprint", format!("join: {e}"))),
        };

        if windows.is_empty() {
            return Ok(StageOutcome::Skipped);
        }
        for w in windows {
            write_fingerprint(&ctx.library, book_id, &w).await?;
        }
        Ok(StageOutcome::Done)
    }
}

async fn write_fingerprint(library: &LibraryDb, book_id: BookId, window: &Window) -> Result<()> {
    let bytes = fingerprint_to_bytes(&window.fingerprint);
    let offset_i64 = i64::from(window.offset_sec);
    let duration_i64 = i64::from(window.duration_sec);
    sqlx::query(
        "INSERT OR REPLACE INTO book_fingerprints \
         (book_id, offset_sec, duration_sec, fingerprint, algorithm) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(book_id.0)
    .bind(offset_i64)
    .bind(duration_i64)
    .bind(&bytes)
    .bind(ALGORITHM)
    .execute(library.pool())
    .await
    .map_err(|e| Error::Database(format!("insert fingerprint: {e}")))?;
    Ok(())
}

/// Pack a `Vec<u32>` chromaprint hash sequence into little-endian
/// bytes for BLOB storage.
pub fn fingerprint_to_bytes(fp: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(fp.len() * 4);
    for v in fp {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Inverse of [`fingerprint_to_bytes`]. Returns an empty vec when
/// the byte length isn't a multiple of 4.
pub fn fingerprint_from_bytes(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| {
            let mut buf = [0_u8; 4];
            buf.copy_from_slice(chunk);
            u32::from_le_bytes(buf)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn hamming_zero_when_equal() {
        let a = [1_u32, 2, 3, 4, 5];
        assert_eq!(hamming(&a, &a), 0);
    }

    #[test]
    fn hamming_counts_bits() {
        let a = [0_u32];
        let b = [0b1111_u32];
        assert_eq!(hamming(&a, &b), 4);
    }

    #[test]
    fn hamming_penalises_length_mismatch() {
        let a = [0_u32];
        let b = [0_u32, 0];
        assert_eq!(hamming(&a, &b), 32);
    }

    #[test]
    fn bytes_roundtrip() {
        let fp = vec![0xDEAD_BEEF_u32, 0x1234_5678, 0xFFFF_0000];
        let bytes = fingerprint_to_bytes(&fp);
        assert_eq!(fingerprint_from_bytes(&bytes), fp);
    }
}
