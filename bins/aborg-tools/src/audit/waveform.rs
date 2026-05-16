//! Pure-Rust SVG waveform renderer for the audiologo-audit
//! binary (ADR-0054).
//!
//! Decode an audio clip to PCM via `symphonia`, downsample to
//! a target number of buckets, and emit an inline SVG with one
//! `<rect>` per bucket. Optionally overlays a vertical cut
//! marker.
//!
//! No external SVG library dep — every shape is a `format!`'d
//! `<rect/>` or `<line/>`. The output goes directly into the
//! HTML report's `<section>` for that book.

#![allow(
    clippy::format_push_string,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::needless_raw_string_hashes,
    clippy::manual_let_else,
    clippy::default_trait_access
)]

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Default waveform width in SVG user units.
pub const WAVEFORM_WIDTH: u32 = 1200;
/// Default waveform height in SVG user units.
pub const WAVEFORM_HEIGHT: u32 = 200;

/// Render an SVG waveform for the audio file at `path`.
///
/// `cut_offset_ms` overlays a vertical red line at the
/// proposed cut position (relative to the file start). Pass
/// `None` to skip the marker.
///
/// # Errors
///
/// Returns an error if the file can't be opened or decoded.
pub fn render(path: &Path, cut_offset_ms: Option<u64>) -> Result<String> {
    let mono =
        decode_to_mono(path).with_context(|| format!("decode waveform for {}", path.display()))?;
    if mono.samples.is_empty() {
        return Ok(empty_svg());
    }
    let buckets = downsample(&mono.samples, WAVEFORM_WIDTH as usize);
    Ok(emit_svg(&buckets, mono.duration_ms, cut_offset_ms))
}

struct MonoPcm {
    samples: Vec<f32>,
    duration_ms: u64,
}

fn decode_to_mono(path: &Path) -> Result<MonoPcm> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(std::ffi::OsStr::to_str) {
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| "symphonia probe")?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("no default audio track in {}", path.display()))?;
    let track_id = track.id;
    let Some(CodecParameters::Audio(audio_params)) = track.codec_params.clone() else {
        return Err(anyhow!("track has no audio params"));
    };
    let sample_rate = audio_params.sample_rate.unwrap_or(44_100);
    let channels = audio_params
        .channels
        .as_ref()
        .map_or(2_usize, symphonia::core::audio::Channels::count);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .with_context(|| "make decoder")?;

    let mut samples: Vec<f32> = Vec::new();
    while let Some(packet) = format.next_packet().with_context(|| "read packet")? {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let mut interleaved: Vec<f32> = Vec::with_capacity(decoded.samples_interleaved());
        decoded.copy_to_vec_interleaved::<f32>(&mut interleaved);
        if channels <= 1 {
            samples.extend_from_slice(&interleaved);
        } else {
            // Average channels to mono.
            #[allow(clippy::cast_precision_loss)]
            let inv = 1.0_f32 / channels as f32;
            for frame in interleaved.chunks_exact(channels) {
                let s: f32 = frame.iter().sum::<f32>() * inv;
                samples.push(s);
            }
        }
    }

    let duration_ms = if sample_rate == 0 {
        0
    } else {
        u64::try_from((samples.len() as u128 * 1000) / u128::from(sample_rate)).unwrap_or(u64::MAX)
    };

    Ok(MonoPcm {
        samples,
        duration_ms,
    })
}

/// Downsample to `target_buckets` by computing RMS of each
/// bucket's samples. Output is clamped to `[0.0, 1.0]`.
fn downsample(samples: &[f32], target_buckets: usize) -> Vec<f32> {
    if target_buckets == 0 || samples.is_empty() {
        return Vec::new();
    }
    let per_bucket = samples.len().div_ceil(target_buckets);
    let mut out = Vec::with_capacity(target_buckets);
    let mut idx = 0;
    while idx < samples.len() {
        let end = (idx + per_bucket).min(samples.len());
        let slice = &samples[idx..end];
        #[allow(clippy::cast_precision_loss)]
        let sum_sq: f32 = slice.iter().map(|s| s * s).sum();
        #[allow(clippy::cast_precision_loss)]
        let rms = (sum_sq / slice.len() as f32).sqrt();
        out.push(rms.clamp(0.0, 1.0));
        idx = end;
    }
    out
}

fn emit_svg(buckets: &[f32], duration_ms: u64, cut_offset_ms: Option<u64>) -> String {
    let w = WAVEFORM_WIDTH;
    let h = WAVEFORM_HEIGHT;
    let half_h = h / 2;
    let n = u32::try_from(buckets.len()).unwrap_or(u32::MAX);
    let bar_w = if n == 0 { 1 } else { (w / n.max(1)).max(1) };

    let mut svg = String::with_capacity(8 * 1024);
    svg.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="100%" preserveAspectRatio="xMidYMid meet" role="img" aria-label="audio waveform">"##
    ));
    svg.push_str(&format!(
        r##"<rect width="{w}" height="{h}" fill="#f5f7fa"/>"##
    ));
    svg.push_str(&format!(
        r##"<line x1="0" y1="{half_h}" x2="{w}" y2="{half_h}" stroke="#cbd5e0" stroke-width="1"/>"##
    ));

    for (i, &v) in buckets.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let bar_h = ((half_h as f32) * v).max(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bar_h_u = bar_h as u32;
        let x = u32::try_from(i).unwrap_or(u32::MAX) * bar_w;
        let y = half_h.saturating_sub(bar_h_u);
        let bar_total = bar_h_u * 2;
        svg.push_str(&format!(
            r##"<rect x="{x}" y="{y}" width="{bar_w}" height="{bar_total}" fill="#2563eb"/>"##
        ));
    }

    if let Some(offset_ms) = cut_offset_ms {
        if duration_ms > 0 {
            let x = (u128::from(offset_ms) * u128::from(w) / u128::from(duration_ms))
                .min(u128::from(w));
            svg.push_str(&format!(
                r##"<line x1="{x}" y1="0" x2="{x}" y2="{h}" stroke="#dc2626" stroke-width="2"/>"##
            ));
            svg.push_str(&format!(
                r##"<text x="{}" y="14" fill="#dc2626" font-family="monospace" font-size="11">{} ms</text>"##,
                x.saturating_sub(50),
                offset_ms
            ));
        }
    }

    svg.push_str("</svg>");
    svg
}

fn empty_svg() -> String {
    let w = WAVEFORM_WIDTH;
    let h = WAVEFORM_HEIGHT;
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="100%"><rect width="{w}" height="{h}" fill="#f5f7fa"/><text x="20" y="100" font-family="monospace" font-size="14" fill="#94a3b8">empty / decode failed</text></svg>"##
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn downsample_empty_returns_empty() {
        assert!(downsample(&[], 100).is_empty());
    }

    #[test]
    fn downsample_to_zero_buckets_returns_empty() {
        assert!(downsample(&[0.5, 0.5], 0).is_empty());
    }

    #[test]
    fn downsample_rms_is_correct_for_uniform_input() {
        let samples = vec![0.5_f32; 1000];
        let out = downsample(&samples, 10);
        assert_eq!(out.len(), 10);
        for v in out {
            assert!((v - 0.5).abs() < 0.001);
        }
    }

    #[test]
    fn empty_svg_renders_with_message() {
        let s = empty_svg();
        assert!(s.contains("decode failed"));
        assert!(s.contains("<svg"));
    }
}
