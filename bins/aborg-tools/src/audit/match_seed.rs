//! Seed-fingerprint match wiring for the audit binary
//! (ADR-0054, Phase 2C, revised after operator caught the
//! drift: ABtagger fingerprints are **RMS thumbprints +
//! cosine similarity**, not chromaprint).
//!
//! ## Why RMS + cosine (not chromaprint)
//!
//! Chromaprint buys bitrate / codec invariance — exactly what
//! you want for *duplicate detection* (same recording at
//! different qualities) and exactly what slice 1C / the
//! `ab-fingerprint` crate exists for. We don't need that for
//! audiologos. Audiologos are deterministic — same publisher,
//! same recording, every book. ABtagger's
//! `crates/.../src/audio/fingerprint.rs` makes the choice
//! explicit:
//!
//! > *"we don't need full chromaprint-style invariance to
//! > bitrate or codec; we need an inexpensive way to ask
//! > 'does the first 30 s of this M4B match a known intro?'."*
//!
//! Method: decode the clip into mono f32 PCM, take one RMS
//! value per **100 ms** window, compare via **cosine
//! similarity**. Storage: ~1.2 KiB per 30 s thumbprint
//! (300 × f32). Match cost: ~few µs per seed.
//!
//! ## What the seed format actually is
//!
//! `intro_fingerprint_b64` / `outro_fingerprint_b64` in
//! ABtagger's `audiologo_findings_*.json` is the base64
//! encoding of the LE-packed `f32` RMS thumbprint, *not*
//! a chromaprint hash sequence. A 2.6 s intro thumbprint
//! is 26 × 4 = 104 bytes ≈ 140 base64 chars; `intro_fingerprint_duration_ms`
//! tells us the source span in ms (= `count × WINDOW_MS`).
//!
//! ## Match pipeline
//!
//! 1. Compute the audit clip's RMS thumbprint (60 s →
//!    600 windows).
//! 2. Narrow seeds to position + publisher-compatible.
//! 3. Slide each seed across the clip's thumbprint; at each
//!    offset compute cosine similarity over the seed's
//!    window length.
//! 4. Return the highest-cosine alignment.
//!
//! ## Threshold
//!
//! The audit binary surfaces the *best* candidate per side
//! without a minimum threshold — the operator visually
//! confirms in the report. Downstream production consumers
//! compare against a per-method threshold (ABtagger's
//! production code uses ~0.85 with per-logo overrides).

#![allow(
    clippy::missing_errors_doc,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::too_long_first_doc_paragraph,
    clippy::similar_names,
    clippy::float_cmp
)]

use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use super::seed::{Position, SeedDb};

/// RMS window size, in milliseconds. Matches ABtagger's
/// `WINDOW_MS = 100` — and crucially, what the seed
/// thumbprints were computed against. Don't change without
/// re-fingerprinting every seed.
pub const WINDOW_MS: u32 = 100;

/// Best-match result against the seed DB.
#[derive(Debug, Clone)]
pub struct SeedMatch {
    /// Index into [`SeedDb::fingerprints`] — useful for fetching
    /// the publisher / transcript excerpt for display.
    pub seed_idx: usize,
    /// Publisher tag from the matched seed.
    pub publisher: Option<String>,
    /// Publisher-match label from the matched seed.
    pub publisher_match: Option<String>,
    /// Position the match anchored at.
    pub position: Position,
    /// RMS-window index where the best alignment starts (each
    /// window = [`WINDOW_MS`] ms).
    pub window_offset: usize,
    /// Cosine similarity ∈ \[-1, 1\] at that offset. In practice
    /// audiologo matches sit > 0.9; values around 0.5-0.7
    /// indicate a partial / spurious match.
    pub confidence: f32,
    /// Number of RMS windows in the seed (needle length).
    pub seed_windows: usize,
}

/// Match `clip_path` against every position-compatible,
/// publisher-compatible seed in `seeds`. Returns the best
/// match or `None` if no seed matches.
///
/// # Errors
///
/// Thumbprinting failures (file missing, unsupported codec,
/// zero samples produced) surface as anyhow.
pub fn best_match(
    clip_path: &Path,
    seeds: &SeedDb,
    publisher_hint: Option<&str>,
    position: Position,
) -> Result<Option<SeedMatch>> {
    let clip_thumb = compute_thumbprint_file(clip_path)
        .with_context(|| format!("thumbprint clip {}", clip_path.display()))?;
    Ok(best_match_against_thumbprint(
        &clip_thumb,
        seeds,
        publisher_hint,
        position,
    ))
}

/// Pure-function variant of [`best_match`] taking an
/// already-computed clip thumbprint. Useful for tests + any
/// caller that has the thumbprint in hand.
#[must_use]
pub fn best_match_against_thumbprint(
    clip_thumb: &[f32],
    seeds: &SeedDb,
    publisher_hint: Option<&str>,
    position: Position,
) -> Option<SeedMatch> {
    let mut best: Option<SeedMatch> = None;
    for (idx, seed) in seeds.fingerprints.iter().enumerate() {
        if seed.position != position {
            continue;
        }
        if !publisher_compatible(publisher_hint, seed.publisher.as_deref()) {
            continue;
        }
        let Some(needle) = decode_thumbprint(&seed.fingerprint_b64) else {
            continue;
        };
        if needle.is_empty() || needle.len() > clip_thumb.len() {
            continue;
        }
        let Some((offset, sim)) = best_cosine_offset(clip_thumb, &needle) else {
            continue;
        };
        let candidate = SeedMatch {
            seed_idx: idx,
            publisher: seed.publisher.clone(),
            publisher_match: seed.publisher_match.clone(),
            position,
            window_offset: offset,
            confidence: sim,
            seed_windows: needle.len(),
        };
        best = match best {
            None => Some(candidate),
            Some(cur) if candidate.confidence > cur.confidence => Some(candidate),
            cur => cur,
        };
    }
    best
}

/// Slide `needle` across `haystack` and return the offset that
/// maximises cosine similarity, along with that similarity.
/// Returns `None` if `needle` won't fit (empty or longer than
/// `haystack`).
fn best_cosine_offset(haystack: &[f32], needle: &[f32]) -> Option<(usize, f32)> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let last_offset = haystack.len() - needle.len();
    let mut best: Option<(usize, f32)> = None;
    for offset in 0..=last_offset {
        let window = &haystack[offset..offset + needle.len()];
        let sim = cosine(window, needle);
        match best {
            None => best = Some((offset, sim)),
            Some((_, prev_sim)) if sim > prev_sim => best = Some((offset, sim)),
            _ => {}
        }
    }
    best
}

/// Cosine similarity in `[-1, 1]`. Returns 0 when either side
/// is all-zero or empty. Trims to the shorter length so a
/// slight scan-duration mismatch doesn't sink the score.
/// Ported verbatim from ABtagger's `audio::fingerprint::cosine`
/// so seed thumbprints score the same way they did at compute
/// time.
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let (mut num, mut da, mut db) = (0.0_f64, 0.0_f64, 0.0_f64);
    for i in 0..n {
        let av = f64::from(a[i]);
        let bv = f64::from(b[i]);
        num += av * bv;
        da += av * av;
        db += bv * bv;
    }
    if da == 0.0 || db == 0.0 {
        return 0.0;
    }
    (num / (da.sqrt() * db.sqrt())) as f32
}

/// Compute the full-file RMS thumbprint of `path` at
/// [`WINDOW_MS`] granularity. Ported from ABtagger's
/// `audio::fingerprint::compute_thumbprint` and adapted to
/// symphonia 0.6's `default_track` / `make_audio_decoder` /
/// `GenericAudioBufferRef::copy_to_vec_interleaved` API.
///
/// Audit clips are pre-cut, so we thumbprint the whole file
/// (no `start_sec` / `duration_sec` slicing).
pub fn compute_thumbprint_file(path: &Path) -> Result<Vec<f32>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("open {} for thumbprint", path.display()))?;
    let mss = MediaSourceStream::new(
        Box::new(file),
        symphonia::core::io::MediaSourceStreamOptions::default(),
    );

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("symphonia: probe failed")?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow::anyhow!("no default audio track in {}", path.display()))?;
    let track_id = track.id;
    let Some(CodecParameters::Audio(audio_params)) = track.codec_params.clone() else {
        anyhow::bail!("track has no audio params in {}", path.display());
    };
    let sample_rate = audio_params
        .sample_rate
        .ok_or_else(|| anyhow::anyhow!("no sample rate"))?;
    let channels = audio_params
        .channels
        .as_ref()
        .map_or(2, symphonia::core::audio::Channels::count);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| anyhow::anyhow!("codec: {e}"))?;

    let win_samples = (f64::from(sample_rate) * f64::from(WINDOW_MS) / 1000.0) as usize;
    let mut window_acc: Vec<f32> = Vec::with_capacity(win_samples);
    let mut thumbnail: Vec<f32> = Vec::new();

    // 0.6: `next_packet()` returns `Ok(Option<Packet>)`.
    while let Some(packet) = format
        .next_packet()
        .map_err(|e| anyhow::anyhow!("read packet: {e}"))?
    {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error = %e, "match_seed.decode_packet_skipped");
                continue;
            }
        };

        // Pull interleaved i16 samples for the whole decoded
        // packet; convert to f32 on the fly. Matches the pattern
        // in `ab_fingerprint::decoded_to_interleaved_i16`.
        let mut interleaved: Vec<i16> = Vec::with_capacity(decoded.samples_interleaved());
        decoded.copy_to_vec_interleaved::<i16>(&mut interleaved);

        let frames = interleaved.len() / channels.max(1);
        for frame_idx in 0..frames {
            let base = frame_idx * channels.max(1);
            let mono_i16_sum: i32 = interleaved[base..base + channels.max(1)]
                .iter()
                .map(|s| i32::from(*s))
                .sum();
            let mono_f32 = (mono_i16_sum as f32) / (channels.max(1) as f32) / 32_768.0;
            window_acc.push(mono_f32);
            if window_acc.len() >= win_samples {
                let rms = (window_acc
                    .iter()
                    .map(|s| f64::from(*s).powi(2))
                    .sum::<f64>()
                    / window_acc.len() as f64)
                    .sqrt() as f32;
                thumbnail.push(rms);
                window_acc.clear();
            }
        }
    }

    if thumbnail.is_empty() {
        anyhow::bail!("thumbprint of {} produced no samples", path.display());
    }
    Ok(thumbnail)
}

/// Decode `s` as standard base64, then unpack as little-endian
/// `f32` RMS values. Returns `None` if either step fails (or
/// the byte length isn't a multiple of 4).
fn decode_thumbprint(s: &str) -> Option<Vec<f32>> {
    let bytes = STANDARD.decode(s).ok()?;
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let mut buf = [0_u8; 4];
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    Some(out)
}

/// Pack a `Vec<f32>` thumbprint to its LE-byte representation
/// (inverse of [`decode_thumbprint`] minus the base64 step).
/// Exposed for tests that need to craft seed fixtures.
#[must_use]
pub fn thumbprint_to_bytes(fp: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(fp.len() * 4);
    for v in fp {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// True when the book's publisher tag plausibly matches the
/// seed's. Both lowercased; either may be a substring of the
/// other. Both must be present — we don't speculatively match
/// "(no publisher)" books against any seed.
fn publisher_compatible(book: Option<&str>, seed: Option<&str>) -> bool {
    let (Some(b), Some(s)) = (book, seed) else {
        return false;
    };
    let bl = b.trim().to_lowercase();
    let sl = s.trim().to_lowercase();
    if bl.is_empty() || sl.is_empty() {
        return false;
    }
    bl == sl || bl.contains(&sl) || sl.contains(&bl)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::audit::seed::{SeedFingerprint, SeedSource};

    fn fixture_seed(
        publisher: Option<&str>,
        position: Position,
        thumbprint: &[f32],
    ) -> SeedFingerprint {
        let bytes = thumbprint_to_bytes(thumbprint);
        let b64 = STANDARD.encode(&bytes);
        SeedFingerprint {
            publisher: publisher.map(str::to_owned),
            publisher_match: None,
            position,
            fingerprint_b64: b64,
            duration_ms: u32::try_from(thumbprint.len()).unwrap_or(0) * WINDOW_MS,
            transcript_excerpt: None,
            source: SeedSource::OperatorConfirmed,
            confirmed: false,
        }
    }

    fn db(seeds: Vec<SeedFingerprint>) -> SeedDb {
        SeedDb {
            fingerprints: seeds,
        }
    }

    #[test]
    fn cosine_identical_vector_is_one() {
        let v = vec![0.5_f32, 0.7, 0.3, 0.9];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 0.0];
        assert!(cosine(&a, &b).abs() < 1e-5);
    }

    #[test]
    fn cosine_zero_vector_safe() {
        assert_eq!(cosine(&[0.0, 0.0, 0.0], &[1.0, 2.0, 3.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_trims_to_shorter() {
        // Identical prefix, mismatched lengths -> 1.0.
        let a = vec![1.0_f32, 2.0, 3.0, 4.0];
        let b = vec![1.0_f32, 2.0, 3.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn decode_thumbprint_round_trip() {
        let original: Vec<f32> = vec![0.1, 0.5, -0.3, 0.9, 0.0];
        let bytes = thumbprint_to_bytes(&original);
        let b64 = STANDARD.encode(&bytes);
        let decoded = decode_thumbprint(&b64).expect("decode");
        assert_eq!(decoded.len(), original.len());
        for (orig, dec) in original.iter().zip(decoded.iter()) {
            assert!(
                (orig - dec).abs() < 1e-6,
                "round-trip diff: {orig} vs {dec}"
            );
        }
    }

    #[test]
    fn decode_thumbprint_rejects_truncated_bytes() {
        // 5 bytes is not a multiple of 4 (sizeof f32).
        let bad_b64 = STANDARD.encode([0_u8; 5]);
        assert!(decode_thumbprint(&bad_b64).is_none());
    }

    #[test]
    fn decode_thumbprint_rejects_invalid_b64() {
        assert!(decode_thumbprint("not-valid-base64-!!").is_none());
    }

    #[test]
    fn publisher_compatible_case_insensitive_substring() {
        assert!(publisher_compatible(
            Some("Audible Originals"),
            Some("audible")
        ));
        assert!(publisher_compatible(
            Some("audible"),
            Some("Audible Originals")
        ));
        assert!(!publisher_compatible(
            Some("Brilliance Audio"),
            Some("Audible")
        ));
        assert!(!publisher_compatible(None, Some("Audible")));
        assert!(!publisher_compatible(Some("Audible"), None));
        assert!(!publisher_compatible(Some("   "), Some("Audible")));
    }

    #[test]
    fn best_cosine_offset_finds_exact_match() {
        let needle = vec![0.5_f32, 0.7, 0.3, 0.9];
        let mut hay = vec![0.1_f32; 20];
        // Inject needle at offset 5.
        for (i, v) in needle.iter().enumerate() {
            hay[5 + i] = *v;
        }
        let (offset, sim) = best_cosine_offset(&hay, &needle).expect("match");
        assert_eq!(offset, 5);
        assert!(sim > 0.99, "near-perfect at exact offset, got {sim}");
    }

    #[test]
    fn best_cosine_offset_returns_none_when_needle_too_long() {
        let needle = vec![0.5_f32; 100];
        let hay = vec![0.5_f32; 50];
        assert!(best_cosine_offset(&hay, &needle).is_none());
    }

    #[test]
    fn best_match_picks_publisher_compatible_only() {
        let needle = vec![0.5_f32, 0.7, 0.3, 0.9, 0.4, 0.6];
        let mut clip = vec![0.0_f32; 50];
        for (i, v) in needle.iter().enumerate() {
            clip[10 + i] = *v;
        }
        let audible_seed = fixture_seed(Some("Audible Originals"), Position::Intro, &needle);
        let brilliance_seed = fixture_seed(Some("Brilliance Audio"), Position::Intro, &needle);
        let outro_seed = fixture_seed(Some("Audible"), Position::Outro, &needle);
        let seeds = db(vec![audible_seed, brilliance_seed, outro_seed]);

        let m = best_match_against_thumbprint(&clip, &seeds, Some("Audible"), Position::Intro)
            .expect("match");
        assert_eq!(m.seed_idx, 0);
        assert_eq!(m.window_offset, 10);
        assert!(m.confidence > 0.99, "perfect match: {}", m.confidence);
        assert_eq!(m.seed_windows, 6);
    }

    #[test]
    fn best_match_returns_none_when_position_wrong() {
        let needle = vec![0.2_f32, 0.4, 0.6, 0.8];
        let clip = needle
            .iter()
            .chain(needle.iter())
            .copied()
            .collect::<Vec<_>>();
        let seeds = db(vec![fixture_seed(
            Some("Audible"),
            Position::Outro,
            &needle,
        )]);
        let m = best_match_against_thumbprint(&clip, &seeds, Some("Audible"), Position::Intro);
        assert!(m.is_none());
    }

    #[test]
    fn best_match_returns_none_when_publisher_hint_absent() {
        let needle = vec![0.3_f32, 0.6, 0.9];
        let clip = needle.clone();
        let seeds = db(vec![fixture_seed(
            Some("Audible"),
            Position::Intro,
            &needle,
        )]);
        let m = best_match_against_thumbprint(&clip, &seeds, None, Position::Intro);
        assert!(m.is_none());
    }

    #[test]
    fn best_match_picks_higher_confidence_when_multiple() {
        // needle_a is what's actually present in the clip; needle_b
        // is a shifted-magnitude version that won't score as well.
        let needle_a = vec![0.5_f32, 0.7, 0.3, 0.9, 0.4];
        let needle_b = vec![0.5_f32, 0.2, 0.8, 0.1, 0.9];
        let mut clip = vec![0.0_f32; 30];
        for (i, v) in needle_a.iter().enumerate() {
            clip[5 + i] = *v;
        }
        let seeds = db(vec![
            fixture_seed(Some("Audible"), Position::Intro, &needle_b), // worse match
            fixture_seed(Some("Audible"), Position::Intro, &needle_a), // exact match
        ]);
        let m = best_match_against_thumbprint(&clip, &seeds, Some("Audible"), Position::Intro)
            .expect("match");
        assert_eq!(m.seed_idx, 1, "exact-match seed must win");
        assert!(m.confidence > 0.99);
    }
}
