//! Whole-book identity fingerprinting via chromaprint.
//!
//! Use this crate to answer: *"is this audio file the same recording
//! as that one?"* — robust to bitrate/codec/format changes, weak to
//! truly different recordings of the same book (different narrator).
//!
//! Separate from [`ab_audiologo`](../ab_audiologo/index.html) — that
//! crate fingerprints short publisher jingles (intro/outro) for trim
//! detection. Different problem, different algorithm.
//!
//! # Approach
//!
//! Compute chromaprint at four offsets (`0%`, `25%`, `50%`, `75%`)
//! of `duration_secs`-second windows. A duplicate match requires
//! agreement at ≥ 2 offsets within Hamming distance `MATCH_HD`.
//!
//! # Storage
//!
//! Fingerprints persist to `book_fingerprints` in `library.db`.
//! Multiple per book (one per offset) so partial-overlap detection
//! works (e.g. abridged vs full).

use std::path::Path;

use ab_core::Result;

/// Number of seconds per fingerprint window.
pub const DEFAULT_WINDOW_SECS: u32 = 30;

/// Relative offsets we fingerprint at, expressed as
/// `position = total_duration_secs * offset_fraction`.
pub const OFFSET_FRACTIONS: &[f64] = &[0.0, 0.25, 0.50, 0.75];

/// Hamming-distance threshold for "same recording".
/// Tuned empirically for chromaprint v2 outputs; verify on fixtures.
pub const MATCH_HD: u32 = 32;

/// A computed fingerprint for one window of one file.
#[derive(Debug, Clone)]
pub struct Window {
    /// Offset in seconds from the start of the file.
    pub offset_sec: u32,
    /// Window duration in seconds.
    pub duration_sec: u32,
    /// Packed chromaprint hash sequence. Algorithm-specific opaque bytes.
    pub fingerprint: Vec<u8>,
}

/// Fingerprint `file` at the standard offsets, returning every window
/// we successfully computed. Empty result means the file was too short
/// or unreadable.
///
/// # Errors
///
/// Returns [`ab_core::Error::Io`] on file-system errors, or
/// [`ab_core::Error::Stage`] on decode failures.
#[allow(clippy::missing_const_for_fn)] // future impl is non-const
pub fn fingerprint_file(_file: &Path, _total_duration_secs: u32) -> Result<Vec<Window>> {
    // TODO: wire `rusty_chromaprint` once the audio decode bridge
    // exposes raw PCM windows.
    Ok(Vec::new())
}

/// Hamming distance between two fingerprint byte slices.
/// Returns the count of differing bits.
pub fn hamming(a: &[u8], b: &[u8]) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x ^ *y).count_ones())
        .sum::<u32>()
        // Penalise length mismatch (each missing byte is 8 bits of difference).
        + u32::try_from(a.len().abs_diff(b.len())).unwrap_or(u32::MAX) * 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hamming_is_symmetric() {
        let a = [0b0000_0000_u8];
        let b = [0b1010_1010_u8];
        assert_eq!(hamming(&a, &b), 4);
        assert_eq!(hamming(&b, &a), 4);
    }

    #[test]
    fn hamming_zero_when_equal() {
        let a = [1, 2, 3, 4, 5];
        assert_eq!(hamming(&a, &a), 0);
    }
}
