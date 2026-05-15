//! Magic-byte detection for companion-file formats (ADR-0043).
//!
//! Extensions are *advisory only* — the bytes win. This protects
//! against a `.pdf` that's actually a renamed EPUB and a `.cbr`
//! that's actually a ZIP comic the operator misnamed.
//!
//! Implementation:
//!
//! - The fast tier checks offset-0 signatures (PDF, KFX, LIT,
//!   CBR / RAR, CB7 / 7z, MOBI's offset-60 marker).
//! - Then tar (offset 257 `ustar`).
//! - Then ZIP (offset 0 `PK\x03\x04`) — and inside ZIP we look
//!   at the first entry's filename to distinguish EPUB from CBZ.
//! - Then text-like sniffs (FB2's `<?xml … <FictionBook`, the
//!   `DjVu` `AT&TFORM…DJVU` marker).
//! - Anything else → [`CompanionFormat::Unknown`].

use crate::CompanionFormat;

/// Map a file path's lowercase extension to a default companion format.
///
/// Used as a **pre-filter** in the scanner — if the extension is
/// in the known set, the byte-level [`detect_format`] runs to
/// confirm or override.
///
/// Returns `None` for unknown extensions including `txt` and `md`
/// (which the scanner skips entirely per ADR-0043).
#[must_use]
pub fn is_companion_extension(ext_lower: &str) -> Option<CompanionFormat> {
    match ext_lower {
        "epub" => Some(CompanionFormat::Epub),
        "pdf" => Some(CompanionFormat::Pdf),
        "mobi" => Some(CompanionFormat::Mobi),
        "azw3" | "azw" => Some(CompanionFormat::Azw3),
        "kfx" => Some(CompanionFormat::Kfx),
        "fb2" => Some(CompanionFormat::Fb2),
        "lit" => Some(CompanionFormat::Lit),
        "djvu" | "djv" => Some(CompanionFormat::Djvu),
        "lrf" => Some(CompanionFormat::Lrf),
        "cbz" => Some(CompanionFormat::Cbz),
        "cbr" => Some(CompanionFormat::Cbr),
        "cb7" => Some(CompanionFormat::Cb7),
        "cbt" => Some(CompanionFormat::Cbt),
        _ => None,
    }
}

/// Detect the companion format from raw bytes.
///
/// `bytes` may be the entire file or a leading prefix; the
/// detector reads at most the first ~512 bytes. Caller is
/// responsible for not loading multi-GB files entirely.
#[must_use]
pub fn detect_format(bytes: &[u8]) -> CompanionFormat {
    if bytes.starts_with(b"%PDF") {
        return CompanionFormat::Pdf;
    }
    if bytes.starts_with(b"Rar!\x1a\x07") {
        return CompanionFormat::Cbr;
    }
    if bytes.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return CompanionFormat::Cb7;
    }
    if bytes.starts_with(b"\xeaDRMION!") {
        return CompanionFormat::Kfx;
    }
    if bytes.starts_with(b"ITOLITLS") {
        return CompanionFormat::Lit;
    }

    // MOBI header byte 60..68 = "BOOKMOBI". AZW3 shares the
    // signature; the version-byte discriminator is out of scope
    // for the foundation slice (operators rarely have both,
    // and parse_tier_for() lumps them together as `ebook_opaque`
    // anyway). When that distinction matters, decode EXTH.
    if bytes.len() >= 68 && &bytes[60..68] == b"BOOKMOBI" {
        return CompanionFormat::Mobi;
    }

    // tar — "ustar" at offset 257.
    if bytes.len() >= 263 && &bytes[257..262] == b"ustar" {
        return CompanionFormat::Cbt;
    }

    // ZIP — offset 0 "PK\x03\x04". Inside ZIP, EPUB has a
    // `mimetype` entry as the first file containing
    // `application/epub+zip`. Without parsing the whole ZIP
    // we look for the literal "mimetype" + epub mime marker
    // bytes within the first 256 bytes — sufficient for any
    // EPUB written by a spec-compliant builder (the mimetype
    // entry MUST come first per the EPUB OCF spec).
    if bytes.starts_with(b"PK\x03\x04") {
        let head = &bytes[..bytes.len().min(256)];
        if find_subsequence(head, b"mimetype").is_some()
            && find_subsequence(head, b"application/epub+zip").is_some()
        {
            return CompanionFormat::Epub;
        }
        return CompanionFormat::Cbz;
    }

    // FB2 — `<?xml` … `<FictionBook` within the first 512 bytes.
    let head = &bytes[..bytes.len().min(512)];
    if head.starts_with(b"<?xml") && find_subsequence(head, b"<FictionBook").is_some() {
        return CompanionFormat::Fb2;
    }

    // DjVu — "AT&TFORM" then "DJVU" within the first 16 bytes.
    if head.starts_with(b"AT&TFORM") && find_subsequence(head, b"DJVU").is_some() {
        return CompanionFormat::Djvu;
    }

    CompanionFormat::Unknown
}

/// Minimal `memchr::find` without pulling the dep. The needle is
/// always short (≤ 32 bytes); naive scan over a ~512-byte
/// haystack is fine.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn pdf_signature_detected() {
        let bytes = b"%PDF-1.7\n%fake";
        assert_eq!(detect_format(bytes), CompanionFormat::Pdf);
    }

    #[test]
    fn cbr_signature_detected() {
        let bytes = b"Rar!\x1a\x07\x00rest";
        assert_eq!(detect_format(bytes), CompanionFormat::Cbr);
    }

    #[test]
    fn cb7_signature_detected() {
        let bytes = b"7z\xbc\xaf\x27\x1crest";
        assert_eq!(detect_format(bytes), CompanionFormat::Cb7);
    }

    #[test]
    fn kfx_signature_detected() {
        let bytes = b"\xeaDRMION!rest";
        assert_eq!(detect_format(bytes), CompanionFormat::Kfx);
    }

    #[test]
    fn lit_signature_detected() {
        let bytes = b"ITOLITLSrest";
        assert_eq!(detect_format(bytes), CompanionFormat::Lit);
    }

    #[test]
    fn mobi_signature_detected_at_offset_60() {
        let mut bytes = vec![0u8; 60];
        bytes.extend_from_slice(b"BOOKMOBI");
        bytes.extend_from_slice(&[0u8; 32]);
        assert_eq!(detect_format(&bytes), CompanionFormat::Mobi);
    }

    #[test]
    fn cbt_tar_ustar_at_offset_257() {
        let mut bytes = vec![0u8; 257];
        bytes.extend_from_slice(b"ustar\0extra padding");
        bytes.resize(1024, 0);
        assert_eq!(detect_format(&bytes), CompanionFormat::Cbt);
    }

    #[test]
    fn zip_with_epub_mimetype_is_epub() {
        // The EPUB OCF spec requires `mimetype` as the first
        // entry, uncompressed, containing exactly
        // "application/epub+zip". The first ~80 bytes of a real
        // EPUB look like: PK + local-file-header for "mimetype"
        // + the 20-byte mime string.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"PK\x03\x04\x14\x00\x00\x00\x00\x00");
        // crude: just splat the keyword + mime in the head
        bytes.extend_from_slice(b"mimetypeapplication/epub+zip");
        bytes.resize(256, 0);
        assert_eq!(detect_format(&bytes), CompanionFormat::Epub);
    }

    #[test]
    fn zip_without_mimetype_is_cbz() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"PK\x03\x04");
        bytes.extend_from_slice(b"page01.jpg");
        bytes.resize(256, 0);
        assert_eq!(detect_format(&bytes), CompanionFormat::Cbz);
    }

    #[test]
    fn fb2_signature_detected() {
        let bytes = b"<?xml version=\"1.0\"?>\n<FictionBook xmlns=\"...\">";
        assert_eq!(detect_format(bytes), CompanionFormat::Fb2);
    }

    #[test]
    fn djvu_signature_detected() {
        let bytes = b"AT&TFORM\x00\x00\x00\x00DJVUmore";
        assert_eq!(detect_format(bytes), CompanionFormat::Djvu);
    }

    #[test]
    fn unknown_bytes_return_unknown() {
        let bytes = b"random garbage with no signature";
        assert_eq!(detect_format(bytes), CompanionFormat::Unknown);
    }

    #[test]
    fn empty_bytes_return_unknown() {
        assert_eq!(detect_format(&[]), CompanionFormat::Unknown);
    }

    #[test]
    fn extension_prefilter_recognises_common_formats() {
        assert_eq!(is_companion_extension("epub"), Some(CompanionFormat::Epub));
        assert_eq!(is_companion_extension("pdf"), Some(CompanionFormat::Pdf));
        assert_eq!(is_companion_extension("mobi"), Some(CompanionFormat::Mobi));
        assert_eq!(is_companion_extension("cbz"), Some(CompanionFormat::Cbz));
        assert_eq!(is_companion_extension("djv"), Some(CompanionFormat::Djvu));
        assert_eq!(is_companion_extension("azw"), Some(CompanionFormat::Azw3));
    }

    #[test]
    fn extension_prefilter_skips_text_and_unknown() {
        // TXT / MD are intentionally NOT companions — they're
        // README / LICENSE noise per ADR-0043.
        assert_eq!(is_companion_extension("txt"), None);
        assert_eq!(is_companion_extension("md"), None);
        assert_eq!(is_companion_extension("readme"), None);
        assert_eq!(is_companion_extension("zip"), None);
    }
}
