//! Pixel-bomb defence (slice B.2b → ADR-0030).
//!
//! CDN cover responses, EPUB covers, and PDF first-page rasters
//! can claim a tiny on-disk footprint and decode to a multi-GB
//! raster. The defence is to read the image header without
//! allocating a decoder buffer, refuse anything outside the
//! per-format byte cap or beyond the configured pixel cap, and
//! only then decode.
//!
//! Two entry points:
//!
//! - [`probe_dimensions`] — header-only read. Useful when the
//!   caller wants to gate further work (e.g. resize-to-thumbnail)
//!   without actually decoding the source pixels.
//! - [`decode_checked`] — probe + decode. The probe is run before
//!   the decoder ever touches the payload; if dimensions are
//!   beyond the cap, the function returns
//!   [`DecodeCheckedError::TooManyPixels`] without allocating
//!   `width * height * channels` bytes.

use image::DynamicImage;
use image::ImageReader;

/// Typed failures from [`decode_checked`] / [`probe_dimensions`].
#[derive(Debug, thiserror::Error)]
pub enum DecodeCheckedError {
    /// Header parse failed — bytes aren't a recognised image, or
    /// the format isn't enabled in the image feature set.
    #[error("decode-checked: header read: {0}")]
    HeaderRead(String),
    /// Bytes exceed the per-decode byte cap.
    #[error("decode-checked: payload too large ({got} > {max} bytes)")]
    TooLarge {
        /// Observed byte count.
        got: usize,
        /// Cap from caller.
        max: usize,
    },
    /// Decoded dimensions exceed the configured pixel cap. The
    /// canonical pixel-bomb signature.
    #[error("decode-checked: too many pixels ({width}x{height} > {max_pixels})")]
    TooManyPixels {
        /// Header-reported width.
        width: u32,
        /// Header-reported height.
        height: u32,
        /// Cap from caller.
        max_pixels: u64,
    },
    /// Header passed; the actual decode (e.g. JPEG entropy stage)
    /// failed.
    #[error("decode-checked: decode: {0}")]
    Decode(String),
}

/// Read the image header only and return `(width, height)`.
///
/// Does NOT allocate a decode buffer. The cap checks below run on
/// the header-reported dimensions; a well-formed but enormous
/// image is rejected before the decoder ever sees it.
///
/// # Errors
///
/// - [`DecodeCheckedError::TooLarge`] if `bytes.len() > max_bytes`.
/// - [`DecodeCheckedError::HeaderRead`] if the bytes aren't a
///   recognised image.
/// - [`DecodeCheckedError::TooManyPixels`] if `width * height >
///   max_pixels`.
pub fn probe_dimensions(
    bytes: &[u8],
    max_bytes: usize,
    max_pixels: u64,
) -> Result<(u32, u32), DecodeCheckedError> {
    if bytes.len() > max_bytes {
        return Err(DecodeCheckedError::TooLarge {
            got: bytes.len(),
            max: max_bytes,
        });
    }
    let cursor = std::io::Cursor::new(bytes);
    let reader = ImageReader::new(cursor)
        .with_guessed_format()
        .map_err(|e| DecodeCheckedError::HeaderRead(e.to_string()))?;
    let (w, h) = reader
        .into_dimensions()
        .map_err(|e| DecodeCheckedError::HeaderRead(e.to_string()))?;
    if u64::from(w) * u64::from(h) > max_pixels {
        return Err(DecodeCheckedError::TooManyPixels {
            width: w,
            height: h,
            max_pixels,
        });
    }
    Ok((w, h))
}

/// Decode `bytes` to a [`DynamicImage`] after enforcing the byte
/// + pixel caps.
///
/// The check happens BEFORE the decoder allocates its raster
/// buffer, so a pixel-bomb image is rejected without the daemon
/// paying for the decode.
///
/// `max_bytes` is the input-size cap (caller already knows it
/// from the HTTP `Content-Length` or the file size). `max_pixels`
/// is the post-decode raster cap (default 200 megapixels — well
/// beyond any legitimate audiobook cover).
///
/// # Errors
///
/// See [`DecodeCheckedError`].
pub fn decode_checked(
    bytes: &[u8],
    max_bytes: usize,
    max_pixels: u64,
) -> Result<DynamicImage, DecodeCheckedError> {
    let _ = probe_dimensions(bytes, max_bytes, max_pixels)?;
    let cursor = std::io::Cursor::new(bytes);
    ImageReader::new(cursor)
        .with_guessed_format()
        .map_err(|e| DecodeCheckedError::HeaderRead(e.to_string()))?
        .decode()
        .map_err(|e| DecodeCheckedError::Decode(e.to_string()))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use image::ImageFormat;

    /// Encode a real `w x h` PNG image. Used only for tests that
    /// need a payload `into_dimensions()` can actually parse.
    ///
    /// Synthesising an oversized-dimensions header without a
    /// full-structure payload doesn't work — `image`'s
    /// `into_dimensions()` decodes enough of the bitstream to
    /// reject truncated images. The tests below pair a real
    /// small image with a tight `max_pixels` cap to prove the
    /// cap-rejection path fires.
    fn real_png(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::new_rgb8(w, h);
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode");
        bytes
    }

    #[test]
    fn probe_rejects_payload_over_byte_cap() {
        let bytes = vec![0u8; 1024];
        let err = probe_dimensions(&bytes, 16, 1_000_000).expect_err("over cap");
        assert!(
            matches!(err, DecodeCheckedError::TooLarge { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn probe_rejects_payload_over_pixel_cap() {
        // 200x200 real PNG; cap set tight so the post-probe
        // count exceeds it. Production caps are ~200 megapixels;
        // this fixture only verifies the cap-check fires.
        let bytes = real_png(200, 200);
        let err = probe_dimensions(&bytes, bytes.len(), 1_000).expect_err("pixel cap must reject");
        assert!(
            matches!(err, DecodeCheckedError::TooManyPixels { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn probe_reads_legitimate_dimensions() {
        // Render a 4x3 JPEG via the `image` crate and probe it.
        let img = DynamicImage::new_rgb8(4, 3);
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode");
        let (w, h) = probe_dimensions(&bytes, bytes.len(), 1_000_000).expect("probe");
        assert_eq!((w, h), (4, 3));
    }

    #[test]
    fn decode_checked_returns_an_image_when_within_caps() {
        let img = DynamicImage::new_rgb8(4, 3);
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode");
        let out = decode_checked(&bytes, bytes.len(), 1_000_000).expect("decode");
        assert_eq!((out.width(), out.height()), (4, 3));
    }

    #[test]
    fn decode_checked_short_circuits_when_pixel_cap_exceeded() {
        // The probe runs before the decoder allocates its raster.
        // A real 200x200 PNG with a tight pixel cap proves that
        // gating fires before decode (the test would still pass
        // on a real pixel-bomb image but is cheaper this way).
        let bytes = real_png(200, 200);
        let err = decode_checked(&bytes, bytes.len(), 1_000).expect_err("must reject");
        assert!(
            matches!(err, DecodeCheckedError::TooManyPixels { .. }),
            "{err:?}"
        );
    }
}
