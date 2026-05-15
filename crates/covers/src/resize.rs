//! Multi-size resize helpers for the future thumbnail cache.
//!
//! The cache itself (LRU eviction, manifest, `covers/<book_id>/<size>.jpg`
//! layout) is a separate slice. The resize primitive lands now so
//! the API is stable when the cache layer arrives.
//!
//! Encoding decision: JPEG at quality 85. Audiobook covers are
//! photographic; JPEG compresses ~10x better than PNG at sizes
//! that are visually indistinguishable. The cache directory is
//! disposable so quality losses don't compound.

use image::ImageFormat;
use image::imageops::FilterType;

use crate::decode::{DecodeCheckedError, decode_checked};

/// Typed resize failures.
#[derive(Debug, thiserror::Error)]
pub enum ResizeError {
    /// Decode of the source bytes failed (or was rejected by the
    /// pixel-bomb guard).
    #[error("resize: decode: {0}")]
    Decode(#[from] DecodeCheckedError),
    /// JPEG encode of the resized raster failed. Practically
    /// unreachable but kept as a typed surface.
    #[error("resize: encode: {0}")]
    Encode(String),
}

/// Resize `bytes` to a square `size_px` JPEG.
///
/// The source is decoded under the pixel-bomb caps below
/// (`max_input_bytes` / `max_input_pixels`); the result is a
/// `size_px × size_px` Lanczos3-resampled JPEG with the
/// short-axis ratio preserved (centred crop).
///
/// `size_px` of 64 / 128 / 256 / 512 are the canonical thumbnail
/// sizes per ADR-0030; the API accepts any value so the menubar
/// app target can request Finder-icon sizes (16 / 32 / etc.)
/// from the same surface.
///
/// # Errors
///
/// See [`ResizeError`].
pub fn resize_to_square_jpeg(
    bytes: &[u8],
    size_px: u32,
    max_input_bytes: usize,
    max_input_pixels: u64,
) -> Result<Vec<u8>, ResizeError> {
    let src = decode_checked(bytes, max_input_bytes, max_input_pixels)?;
    // `resize_to_fill` keeps aspect ratio + centred crop.
    let resized = src.resize_to_fill(size_px, size_px, FilterType::Lanczos3);
    let mut out = Vec::with_capacity((size_px as usize) * (size_px as usize));
    resized
        .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Jpeg)
        .map_err(|e| ResizeError::Encode(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::decode::probe_dimensions;

    fn checker_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h);
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode");
        bytes
    }

    #[test]
    fn resizes_to_square_jpeg_at_each_size() {
        let src = checker_png(800, 1200);
        for size in [64u32, 128, 256, 512] {
            let out = resize_to_square_jpeg(&src, size, src.len(), 100_000_000).expect("resize");
            let (w, h) = probe_dimensions(&out, out.len(), 1_000_000).expect("probe");
            assert_eq!((w, h), (size, size), "size {size}");
        }
    }

    #[test]
    fn rejects_input_over_byte_cap() {
        let src = checker_png(4, 3);
        let err = resize_to_square_jpeg(&src, 64, 4, 1_000_000).expect_err("must reject");
        assert!(
            matches!(
                err,
                ResizeError::Decode(DecodeCheckedError::TooLarge { .. })
            ),
            "{err:?}"
        );
    }
}
