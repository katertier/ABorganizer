//! Companion-file detection + parse-tier mapping (ADR-0043).
//!
//! Sits between the scan stage (which sees a sidecar file path
//! next to an audiobook) and the future C.2 auto-pair logic +
//! C.4 EPUB name-dict extractor + C.5 transcript correction
//! stages. This crate owns:
//!
//! - The [`CompanionFormat`] enum that mirrors the CHECK
//!   constraint on `book_companions.format`.
//! - The [`ParseTier`] enum that mirrors `book_companions.parse_tier`.
//! - [`detect_format`] — magic-byte dispatch. Extensions are
//!   **advisory only**; bytes win.
//! - [`parse_tier_for`] — total mapping from format → tier.
//! - [`is_companion_extension`] — quick path-level filter the
//!   scanner uses to skip ahead. Used as a pre-filter before
//!   the bytes-on-disk check.
//!
//! What's **not** here yet:
//!
//! - Scanner integration / auto-pair geometry rules — C.2.
//! - EPUB name-dict extraction — C.4.
//! - Transcript correction via EPUB dict — C.5.
//! - `StaleCompanionHintsTarget` (tracker #124).
//!
//! All three rely on this crate's enums + detector; landing them
//! first means each follow-up slice is a pure consumer.

mod detect;
mod pair;

pub use detect::{detect_format, is_companion_extension};
pub use pair::{
    AudiobookCandidate, AutoPairResult, auto_pair, is_ancestor_or_equal, path_diverges,
};

use serde::{Deserialize, Serialize};

/// Caller-supplied identifier in [`auto_pair`].
///
/// Any `Clone + Eq` type works — the geometry helper is generic
/// so production code passes `BookId` while tests pass plain
/// integers for readability. Trait alias pattern; no methods.
pub trait BookKey: Clone + Eq {}

impl<T: Clone + Eq> BookKey for T {}

/// Companion-file format. Mirrors the `book_companions.format`
/// CHECK constraint exactly — every variant maps to a single
/// lowercase string token via [`CompanionFormat::as_str`].
///
/// `Unknown` is the catch-all when the magic-byte dispatch finds
/// no signature match. The file is still stored (the operator
/// can manually classify it later) but no parse tier kicks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompanionFormat {
    /// EPUB (text-extractable; C.4 consumes this).
    Epub,
    /// PDF (document; list + download only).
    Pdf,
    /// MOBI (Kindle classic; opaque).
    Mobi,
    /// AZW3 (Kindle KF8; opaque).
    Azw3,
    /// KFX (Kindle KFX; opaque + DRM-heavy).
    Kfx,
    /// FB2 (`FictionBook`; opaque for now).
    Fb2,
    /// Microsoft LIT (opaque).
    Lit,
    /// `DjVu` (opaque).
    Djvu,
    /// Sony LRF (opaque).
    Lrf,
    /// Comic ZIP.
    Cbz,
    /// Comic RAR.
    Cbr,
    /// Comic 7z.
    Cb7,
    /// Comic TAR.
    Cbt,
    /// Bytes-on-disk that didn't match any known signature.
    Unknown,
}

impl CompanionFormat {
    /// Canonical lowercase token. Used as the on-disk SQL value
    /// for `book_companions.format`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Epub => "epub",
            Self::Pdf => "pdf",
            Self::Mobi => "mobi",
            Self::Azw3 => "azw3",
            Self::Kfx => "kfx",
            Self::Fb2 => "fb2",
            Self::Lit => "lit",
            Self::Djvu => "djvu",
            Self::Lrf => "lrf",
            Self::Cbz => "cbz",
            Self::Cbr => "cbr",
            Self::Cb7 => "cb7",
            Self::Cbt => "cbt",
            Self::Unknown => "unknown",
        }
    }
}

/// Parse tier — what (if anything) the pipeline does with the
/// companion's contents.
///
/// `text_extractable` is the only tier the LLM correction stages
/// (C.4 / C.5) consume. Everything else is "store + surface in
/// the UI"; the operator can download the file but the daemon
/// doesn't try to read its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseTier {
    /// EPUB — C.4 extracts a proper-noun dictionary for C.5
    /// transcript correction.
    TextExtractable,
    /// PDF — list + download surface only.
    Document,
    /// MOBI / AZW3 / KFX / FB2 / LIT / DJVU / LRF.
    EbookOpaque,
    /// CBZ / CBR / CB7 / CBT.
    Comic,
    /// Unrecognised bytes.
    Unknown,
}

impl ParseTier {
    /// Canonical `snake_case` token. Stored as
    /// `book_companions.parse_tier`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TextExtractable => "text_extractable",
            Self::Document => "document",
            Self::EbookOpaque => "ebook_opaque",
            Self::Comic => "comic",
            Self::Unknown => "unknown",
        }
    }
}

/// Total mapping from format → parse tier. Closed match —
/// adding a [`CompanionFormat`] variant forces the matching
/// arm here at compile time.
#[must_use]
pub const fn parse_tier_for(format: CompanionFormat) -> ParseTier {
    match format {
        CompanionFormat::Epub => ParseTier::TextExtractable,
        CompanionFormat::Pdf => ParseTier::Document,
        CompanionFormat::Mobi
        | CompanionFormat::Azw3
        | CompanionFormat::Kfx
        | CompanionFormat::Fb2
        | CompanionFormat::Lit
        | CompanionFormat::Djvu
        | CompanionFormat::Lrf => ParseTier::EbookOpaque,
        CompanionFormat::Cbz
        | CompanionFormat::Cbr
        | CompanionFormat::Cb7
        | CompanionFormat::Cbt => ParseTier::Comic,
        CompanionFormat::Unknown => ParseTier::Unknown,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_tier_for_every_format_is_total() {
        // Walk every variant via the as_str round-trip; ensures
        // adding a new variant without a `parse_tier_for` arm
        // fails to compile (the match is exhaustive).
        for fmt in [
            CompanionFormat::Epub,
            CompanionFormat::Pdf,
            CompanionFormat::Mobi,
            CompanionFormat::Azw3,
            CompanionFormat::Kfx,
            CompanionFormat::Fb2,
            CompanionFormat::Lit,
            CompanionFormat::Djvu,
            CompanionFormat::Lrf,
            CompanionFormat::Cbz,
            CompanionFormat::Cbr,
            CompanionFormat::Cb7,
            CompanionFormat::Cbt,
            CompanionFormat::Unknown,
        ] {
            let _tier = parse_tier_for(fmt);
            assert!(!fmt.as_str().is_empty());
        }
    }

    #[test]
    fn epub_maps_to_text_extractable() {
        assert_eq!(
            parse_tier_for(CompanionFormat::Epub),
            ParseTier::TextExtractable
        );
    }

    #[test]
    fn pdf_is_document_tier_not_text() {
        // PDF text-extraction was rejected at design time — the
        // surface is list + download.
        assert_eq!(parse_tier_for(CompanionFormat::Pdf), ParseTier::Document);
    }

    #[test]
    fn opaque_ebooks_share_one_tier() {
        for fmt in [
            CompanionFormat::Mobi,
            CompanionFormat::Azw3,
            CompanionFormat::Kfx,
            CompanionFormat::Fb2,
            CompanionFormat::Lit,
            CompanionFormat::Djvu,
            CompanionFormat::Lrf,
        ] {
            assert_eq!(parse_tier_for(fmt), ParseTier::EbookOpaque, "{fmt:?}");
        }
    }

    #[test]
    fn comics_share_one_tier() {
        for fmt in [
            CompanionFormat::Cbz,
            CompanionFormat::Cbr,
            CompanionFormat::Cb7,
            CompanionFormat::Cbt,
        ] {
            assert_eq!(parse_tier_for(fmt), ParseTier::Comic, "{fmt:?}");
        }
    }
}
