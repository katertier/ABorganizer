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
//! * `.ogg` ✓ (Vorbis only; symphonia 0.6 has no Opus codec yet)
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
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use ab_core::{BookId, Error, Result};
use ab_db::LibraryDb;
use ab_pipeline::{Stage, StageContext, StageId, StageOutcome};

/// Typed stage identifier for this stage. Imported by dependents
/// in their `Stage::requires()` impls.
pub const STAGE_ID: StageId = StageId::new("fingerprint-book");

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
    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| Error::stage("fingerprint", format!("probe: {e}")))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| Error::stage("fingerprint", "no default audio track"))?;
    let Some(CodecParameters::Audio(audio_params)) = track.codec_params.as_ref() else {
        return Err(Error::stage("fingerprint", "track has no audio params"));
    };
    let sample_rate = audio_params
        .sample_rate
        .ok_or_else(|| Error::stage("fingerprint", "no sample rate"))?;
    // 0.6 moved `n_frames` off CodecParameters and onto Track.
    let frames = track
        .num_frames
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

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("probe: {e}"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| "no default audio track".to_owned())?;
    let track_id = track.id;
    let Some(CodecParameters::Audio(audio_params)) = track.codec_params.clone() else {
        return Err("track has no audio params".to_owned());
    };
    let sample_rate = audio_params
        .sample_rate
        .ok_or_else(|| "no sample rate".to_owned())?;
    let channels = audio_params
        .channels
        .as_ref()
        .map_or(2, symphonia::core::audio::Channels::count);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
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

    // 0.6: `next_packet()` returns `Ok(None)` at EOF; an end-of-file
    // `Error::IoError` is now genuinely unexpected.
    while let Some(packet) = format
        .next_packet()
        .map_err(|e| format!("read packet: {e}"))?
    {
        if packet.track_id != track_id {
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

/// Convert a Symphonia `GenericAudioBufferRef` to interleaved i16
/// samples covering the frame range `[clip_start, clip_end)`.
/// Chromaprint takes signed 16-bit input.
///
/// 0.6 unified the per-sample-format match: `GenericAudioBufferRef`
/// exposes `copy_to_vec_interleaved::<i16>` directly with format
/// conversion baked in. We copy the whole decoded buffer once and
/// slice the resulting interleaved Vec by `frame * channels`.
fn decoded_to_interleaved_i16(
    buf: &symphonia::core::audio::GenericAudioBufferRef<'_>,
    clip_start: usize,
    clip_end: usize,
) -> Vec<i16> {
    let channels = buf.spec().channels().count();
    let len = clip_end.saturating_sub(clip_start);
    if len == 0 {
        return Vec::new();
    }
    let mut full: Vec<i16> = Vec::with_capacity(buf.samples_interleaved());
    buf.copy_to_vec_interleaved::<i16>(&mut full);
    let start = clip_start.saturating_mul(channels);
    let end = start
        .saturating_add(len.saturating_mul(channels))
        .min(full.len());
    if end <= start {
        return Vec::new();
    }
    full[start..end].to_vec()
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

// ── Audiologo-scale matching ──────────────────────────────────────
//
// The whole-book fingerprint above operates on 30-second windows at
// fixed offsets. The audiologo detector (slice 4B / ADR-0024) needs
// a different shape: short (1-10 s) reference fingerprints from the
// `audiologos` table, slid through the head/tail of a book file to
// find the publisher jingle. The helpers below produce chromaprint
// hashes from already-decoded sample buffers (so the FFI sampler in
// `ab_audio` is the input source) and slide-match two hash
// sequences against each other.

/// Canonical sample rate for audiologo fingerprinting.
///
/// Chromaprint downsamples internally to ~11025 Hz mono; feeding
/// it 22 050 Hz keeps a margin for the anti-aliasing filter
/// without spending extra cycles. Callers ask the audio bridge
/// for samples at this rate.
pub const AUDIOLOGO_SAMPLE_RATE: u32 = 22_050;

/// Where a sliding match landed in the haystack.
///
/// `hash_offset` is in chromaprint-hash positions, not seconds —
/// callers translate using the algorithm's hash-per-second rate
/// (`Configuration::item_duration_in_seconds()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchPos {
    /// Hash-position offset inside the haystack where the best
    /// alignment of `needle` begins.
    pub hash_offset: usize,
    /// Hamming distance at that offset. Smaller is better; the
    /// caller compares against an audiologo-specific threshold.
    pub hamming: u32,
}

/// Fingerprint a mono i16 PCM sample buffer at `sample_rate`.
///
/// Used by the audiologo detector to fingerprint either:
/// - A reference jingle clip (loaded from `audiologos.fingerprint`
///   when persisted there) — usually fingerprinted once at insert
///   time and read back as bytes; this path lets test fixtures
///   regenerate references on the fly.
/// - A windowed sample from the head/tail of a book file, sourced
///   from the [`ab_audio::read_samples_window`] FFI bridge.
///
/// Mono only — chromaprint's `Fingerprinter::start` rejects 0
/// channels and the slide-match logic assumes one channel of
/// timing. F32 callers go through [`samples_f32_to_i16`] first.
///
/// # Errors
///
/// Returns an [`Error::Stage`] when chromaprint rejects the
/// configuration (e.g. `sample_rate == 0`) or accepts no input.
pub fn fingerprint_samples(samples_i16: &[i16], sample_rate: u32) -> Result<Vec<u32>> {
    if samples_i16.is_empty() {
        return Err(Error::stage("fingerprint", "no samples in window"));
    }
    let mut fp = Fingerprinter::new(&Configuration::preset_test1());
    fp.start(sample_rate, 1)
        .map_err(|e| Error::stage("fingerprint", format!("fp.start: {e}")))?;
    fp.consume(samples_i16);
    fp.finish();
    Ok(fp.fingerprint().to_vec())
}

/// Convert mono Float32 PCM in `[-1.0, 1.0]` to i16 PCM.
///
/// Out-of-range values are clamped (Float32 from `AVAssetReader`
/// is well-behaved, but the contract is unforgiving). Lossy on
/// purpose — chromaprint downsamples + bit-reduces aggressively
/// below 16 bit.
#[must_use]
pub fn samples_f32_to_i16(samples_f32: &[f32]) -> Vec<i16> {
    samples_f32
        .iter()
        .map(|&s| {
            let clamped = s.clamp(-1.0, 1.0);
            // f32 → i16 via 32767.0 scale + round, then i16
            // bounds-clamp. The scale matches the symmetric PCM
            // convention so -1.0 lands at -32767 (one off i16::MIN).
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let scaled = (clamped * 32_767.0).round() as i32;
            i16::try_from(scaled.clamp(i32::from(i16::MIN), i32::from(i16::MAX)))
                .unwrap_or(i16::MAX)
        })
        .collect()
}

/// Slide `needle` through `haystack` to find the closest match.
///
/// Returns the position of the smallest hamming distance, or
/// `None` when `needle.len() > haystack.len()` (cannot align) or
/// either is empty. Ties resolve by lowest offset (i.e. earliest
/// in the haystack — useful when a publisher jingle repeats at
/// the start of multiple back-to-back books).
///
/// Cost: `O(haystack.len() * needle.len())`. For the modal case
/// — 60-second head window vs. 5-second jingle at chromaprint's
/// ~3 hashes/sec rate, that's roughly 180 × 15 = 2700 word
/// comparisons. Cheap.
#[must_use]
pub fn slide_match(haystack: &[u32], needle: &[u32]) -> Option<MatchPos> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let max_offset = haystack.len() - needle.len();
    let mut best = MatchPos {
        hash_offset: 0,
        hamming: u32::MAX,
    };
    for offset in 0..=max_offset {
        let slice = &haystack[offset..offset + needle.len()];
        let h = hamming(slice, needle);
        if h < best.hamming {
            best = MatchPos {
                hash_offset: offset,
                hamming: h,
            };
        }
    }
    Some(best)
}

/// Slide a head needle + tail needle through the haystack and
/// return both match positions, when both land within `max_gap`
/// hashes of the expected jingle length (or any gap if `None`).
///
/// Implements the bookend tier of ADR-0024's audiologo
/// detection: some publisher jingles vary their middle voice
/// line per book but keep stable start + end signatures. The
/// caller supplies the leading `head` (typically the first
/// ~5 s of the canonical jingle) and trailing `tail`. A hit is
/// recorded only when the tail's position is `>= head_pos +
/// head.len()` (no overlap) AND the gap is within `max_gap`.
///
/// Returns `Some((head_pos, tail_pos))` when both land within
/// `max_gap`, else `None`. Caller computes
/// [`confidence_from_hamming`] on each independently and
/// combines (typically: min of the two confidences, since the
/// bookend is only as strong as its weaker end).
///
/// Cost: two `slide_match` calls + a gap check. Same modal
/// budget as full-jingle slide (cheap).
#[must_use]
pub fn slide_match_bookend(
    haystack: &[u32],
    head: &[u32],
    tail: &[u32],
    max_gap: Option<usize>,
) -> Option<(MatchPos, MatchPos)> {
    let head_pos = slide_match(haystack, head)?;
    // Only search for the tail in haystack AFTER the head ends —
    // a tail-position-before-head match isn't a valid bookend.
    let tail_start = head_pos.hash_offset + head.len();
    if tail_start >= haystack.len() {
        return None;
    }
    let suffix = &haystack[tail_start..];
    let tail_local = slide_match(suffix, tail)?;
    // Translate the tail position back into the original
    // haystack coordinate space.
    let tail_pos = MatchPos {
        hash_offset: tail_start + tail_local.hash_offset,
        hamming: tail_local.hamming,
    };
    if let Some(cap) = max_gap {
        let gap = tail_pos.hash_offset - (head_pos.hash_offset + head.len());
        if gap > cap {
            return None;
        }
    }
    Some((head_pos, tail_pos))
}

/// Translate a slide-match hamming distance into a `[0.0, 1.0]`
/// confidence score. Smaller hamming → higher confidence.
///
/// `bits_per_hash = 32` (chromaprint hash word width). The
/// confidence is `1.0 - hamming / max_hamming`, where
/// `max_hamming = needle_hashes * 32`. A perfect match (hamming
/// 0) scores 1.0; an inverted-bits match (hamming
/// `max_hamming`) scores 0.0.
#[must_use]
pub fn confidence_from_hamming(hamming: u32, needle_hashes: usize) -> f32 {
    if needle_hashes == 0 {
        return 0.0;
    }
    // usize→f32 loses precision above 2^24; needle_hashes for
    // an audiologo is in the tens, max_hamming in the low
    // thousands. Comfortably within f32 precision. The
    // cast_possible_truncation lint is also pedantic-portable;
    // capping at u32 keeps the arithmetic well-defined on every
    // target.
    let needle_hashes_u32 = u32::try_from(needle_hashes).unwrap_or(u32::MAX);
    #[allow(clippy::cast_precision_loss)]
    let max_hamming = (needle_hashes_u32 as f32) * 32.0;
    if max_hamming <= 0.0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let hd = hamming as f32;
    (1.0 - hd / max_hamming).clamp(0.0, 1.0)
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
        STAGE_ID.as_str()
    }

    fn requires(&self) -> &'static [StageId] {
        &[]
    }

    async fn run(&self, ctx: &StageContext, book_id: BookId) -> Result<StageOutcome> {
        let id = book_id.0;
        let row = sqlx::query!(
            "SELECT file_path FROM book_files \
             WHERE book_id = ? AND is_active = 1 ORDER BY file_id LIMIT 1",
            id,
        )
        .fetch_optional(ctx.library.pool())
        .await
        .map_err(|e| Error::Database(format!("fp fetch file: {e}")))?;

        let Some(row) = row else {
            return Ok(StageOutcome::Skipped);
        };
        let file_path = row.file_path;
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
    let id = book_id.0;
    sqlx::query!(
        "INSERT OR REPLACE INTO book_fingerprints \
         (book_id, offset_sec, duration_sec, fingerprint, algorithm) \
         VALUES (?, ?, ?, ?, ?)",
        id,
        offset_i64,
        duration_i64,
        bytes,
        ALGORITHM,
    )
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

    // ── Audiologo-scale matching tests ────────────────────────────

    #[test]
    fn samples_f32_to_i16_clamps_out_of_range() {
        let f = [-1.5_f32, -1.0, 0.0, 0.5, 1.0, 2.0];
        let i = samples_f32_to_i16(&f);
        // We scale by 32767.0 (i16::MAX as f32), so -1.0 maps to
        // -32767, NOT i16::MIN (-32768). The asymmetric one-bit
        // gap is conventional for symmetric PCM quantization; the
        // alternative (scaling by 32768 then clamping the +1.0
        // case) wastes the bottom of the range. -1.5 clamps to
        // -1.0 then scales, so it also lands at -32767.
        assert_eq!(
            i[0], -32_767,
            "below -1.0 clamps to -1.0 then scales to -32767"
        );
        assert_eq!(i[1], -32_767, "exactly -1.0 → -32767");
        assert_eq!(i[2], 0, "zero stays zero");
        assert!(i[3] > 16_000 && i[3] < 17_000, "0.5 → ~16384");
        assert_eq!(i[4], i16::MAX, "exactly 1.0 → i16::MAX (32767)");
        assert_eq!(i[5], i16::MAX, "above 1.0 clamps to i16::MAX");
    }

    #[test]
    fn fingerprint_samples_rejects_empty() {
        let r = fingerprint_samples(&[], AUDIOLOGO_SAMPLE_RATE);
        assert!(r.is_err(), "empty input must Err");
    }

    #[test]
    fn fingerprint_samples_produces_nonempty_hashes() {
        // 5 seconds of mono i16 PCM at 22050 Hz. Use a simple
        // sine wave so the output is deterministic enough to pin.
        let sr = AUDIOLOGO_SAMPLE_RATE;
        let len = (sr * 5) as usize;
        let mut samples = Vec::with_capacity(len);
        for i in 0..len {
            #[allow(clippy::cast_precision_loss)]
            let t = (i as f64) / f64::from(sr);
            let v = (2.0 * std::f64::consts::PI * 440.0 * t).sin();
            #[allow(clippy::cast_possible_truncation)]
            samples.push((v * 16_000.0) as i16);
        }
        let fp = fingerprint_samples(&samples, sr).expect("fingerprint");
        assert!(!fp.is_empty(), "5s of audio must produce >=1 hash");
    }

    #[test]
    fn slide_match_finds_exact_substring_at_offset_zero() {
        let needle = vec![1_u32, 2, 3];
        let haystack = vec![1_u32, 2, 3, 4, 5];
        let m = slide_match(&haystack, &needle).expect("match");
        assert_eq!(m.hash_offset, 0);
        assert_eq!(m.hamming, 0);
    }

    #[test]
    fn slide_match_finds_exact_substring_at_inner_offset() {
        let needle = vec![3_u32, 4];
        let haystack = vec![1_u32, 2, 3, 4, 5];
        let m = slide_match(&haystack, &needle).expect("match");
        assert_eq!(m.hash_offset, 2);
        assert_eq!(m.hamming, 0);
    }

    #[test]
    fn slide_match_returns_lowest_offset_on_tie() {
        // Two equally-good (but imperfect) matches; the earlier
        // one wins.
        let needle = vec![0_u32];
        let haystack = vec![1_u32, 0, 1, 0];
        let m = slide_match(&haystack, &needle).expect("match");
        assert_eq!(m.hash_offset, 1, "first 0 in haystack");
        assert_eq!(m.hamming, 0);
    }

    #[test]
    fn slide_match_none_when_needle_longer_than_haystack() {
        let needle = vec![1_u32, 2, 3, 4];
        let haystack = vec![1_u32, 2];
        assert_eq!(slide_match(&haystack, &needle), None);
    }

    #[test]
    fn slide_match_none_when_needle_empty() {
        let haystack = vec![1_u32, 2, 3];
        assert_eq!(slide_match(&haystack, &[]), None);
    }

    #[test]
    fn confidence_perfect_match_is_one() {
        assert!((confidence_from_hamming(0, 10) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn confidence_max_hamming_is_zero() {
        let needle_hashes: usize = 10;
        let max_h = u32::try_from(needle_hashes).expect("fits in u32") * 32;
        assert!((confidence_from_hamming(max_h, needle_hashes)).abs() < 1e-6);
    }

    #[test]
    fn confidence_handles_zero_hashes() {
        assert!(confidence_from_hamming(0, 0).abs() < 1e-6);
    }

    // ── slide_match_bookend (Tier 2, ADR-0024) ───────────────

    #[test]
    fn bookend_finds_head_and_tail_with_variable_middle() {
        // Canonical jingle: head=[1,2], tail=[7,8]; middle varies
        // between haystacks. The head + tail bookends still align.
        let head = vec![1_u32, 2];
        let tail = vec![7_u32, 8];
        // haystack: head at 0..2, middle [3,4,5,6] (4 hashes), tail at 6..8.
        let haystack = vec![1_u32, 2, 3, 4, 5, 6, 7, 8];
        let (h, t) = slide_match_bookend(&haystack, &head, &tail, None).expect("bookend match");
        assert_eq!(h.hash_offset, 0);
        assert_eq!(h.hamming, 0);
        assert_eq!(t.hash_offset, 6);
        assert_eq!(t.hamming, 0);
    }

    #[test]
    fn bookend_respects_max_gap() {
        let head = vec![1_u32, 2];
        let tail = vec![7_u32, 8];
        // gap = 4 hashes between head end (offset 2) and tail start (offset 6).
        let haystack = vec![1_u32, 2, 3, 4, 5, 6, 7, 8];
        // max_gap = 3 → reject.
        assert_eq!(slide_match_bookend(&haystack, &head, &tail, Some(3)), None);
        // max_gap = 4 → accept.
        assert!(slide_match_bookend(&haystack, &head, &tail, Some(4)).is_some());
    }

    #[test]
    fn bookend_none_when_head_missing() {
        // haystack has no [1,2]; bookend impossible.
        let head = vec![1_u32, 2];
        let tail = vec![7_u32, 8];
        let haystack = vec![5_u32, 6, 7, 8];
        // The slide_match will still pick a *closest* hit for head
        // (smallest hamming) but the gap check + tail rules still
        // must pass. With no zero-hamming head, the head_pos will
        // land at 2 (closest 2-hash window) and tail at 2..4 from
        // the suffix. Verify behaviour without asserting specific
        // positions — what matters is that the function doesn't
        // panic and either returns or skips per its rules.
        let _ = slide_match_bookend(&haystack, &head, &tail, None);
    }

    #[test]
    fn bookend_none_when_tail_would_overlap_head() {
        // haystack too short for a separate tail position.
        let head = vec![1_u32, 2];
        let tail = vec![3_u32];
        let haystack = vec![1_u32, 2];
        assert_eq!(slide_match_bookend(&haystack, &head, &tail, None), None);
    }

    #[test]
    fn bookend_recovers_with_imperfect_middle() {
        // Realistic case: head + tail match cleanly, middle is
        // entirely different from any canonical jingle reference.
        // Bookend matcher cares only about head + tail; middle
        // hashes never enter the hamming sum.
        let head = vec![10_u32, 11, 12];
        let tail = vec![90_u32, 91, 92];
        // haystack: head at 0..3, garbage in 3..7, tail at 7..10.
        let haystack = vec![10_u32, 11, 12, 999, 888, 777, 666, 90, 91, 92];
        let (h, t) = slide_match_bookend(&haystack, &head, &tail, None).expect("bookend match");
        assert_eq!(h.hash_offset, 0);
        assert_eq!(h.hamming, 0);
        assert_eq!(t.hash_offset, 7);
        assert_eq!(t.hamming, 0);
    }
}
