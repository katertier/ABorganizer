//! `lofty`-based per-file tag-write helpers.
//!
//! The stage's `run()` decides *what* to write (winners from
//! `book_field_provenance`); this module decides *how* to map
//! those typed winners onto lofty `Tag` items and persist them.
//!
//! ## Field coverage
//!
//! 14 of 16 `book_field_provenance` fields map to a canonical
//! lofty key (slice C3a added `CoverUrl`; slice C3b added
//! `Explicit`). Symmetric with [`ab_tag_read`]'s read path:
//!
//! | `Field`          | lofty mapping                                     |
//! |------------------|---------------------------------------------------|
//! | `Title`          | `Tag::set_title` (typed accessor)                |
//! | `Author`         | `Tag::set_artist` (typed accessor)               |
//! | `Series`         | `Tag::set_album`  (typed accessor; audiobook    |
//! |                  | convention — `Album` carries the series name)    |
//! | `Subtitle`       | `ItemKey::TrackSubtitle` (TIT3)                  |
//! | `Description`    | `ItemKey::Comment` (audiobook synopsis in COMM)  |
//! | `Language`       | `ItemKey::Language`                              |
//! | `ReleaseDate`    | `ItemKey::RecordingDate` (TDRC / Year)           |
//! | `Genre`          | `ItemKey::Genre`                                 |
//! | `Publisher`      | `ItemKey::Publisher`                             |
//! | `Narrator`       | `ItemKey::Composer` (audiobook conv: TCOM)       |
//! | `Asin`           | `ItemKey::CatalogNumber`                         |
//! | `Isbn`           | `ItemKey::Isrc`                                  |
//! | `CoverUrl`       | `Tag::push_picture` (front cover, MIME sniffed   |
//! |                  | from fetched bytes via [`crate::cover`])         |
//! | `Explicit`       | `ItemKey::ParentalAdvisory` — `ITUNESADVISORY`   |
//! |                  | on ID3 / `rtng` atom on MP4. Truthy → `"4"`,    |
//! |                  | falsy → `"0"` (iTunes advisory integer scheme)  |
//!
//! `Field::Abridged` lands via a format-specific dispatch in
//! [`crate::abridged`] — no `ItemKey` exists for it, so it drops
//! below the abstract `Tag` API to typed `Id3v2Tag` (TXXX:ABRIDGED)
//! and `Ilst` freeform-atom (`----:com.apple.iTunes:ABRIDGED`)
//! writers. The cost is a second file open for books with
//! abridged winners; books without one skip the typed path
//! entirely.
//!
//! The remaining unmapped field is:
//!
//! - `DurationSeconds` — derived from decode, not a tag frame.
//!
//! ## Idempotence
//!
//! [`write_winners`] is a no-op when every winner already matches
//! the on-disk value (the "skip when on-disk matches" guard from
//! ADR-0028). This keeps the stage cheap on re-runs and matches
//! the file's `mtime` only when a value actually changed.

use ab_core::{Error, Field, Result};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, Tag};
use std::path::Path;

use crate::winners::FieldWinner;

/// One field's before/after pair recorded for the audit log.
///
/// Surfaced from [`write_winners`] so the stage can mirror each
/// per-field tag mutation into `mass_edit_history`. `before` is
/// `None` when the file had no value for that field prior to
/// the write — the audit row records a creation, not an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldChange {
    /// Which `book_field_provenance` field changed.
    pub field: Field,
    /// Pre-write on-disk value (`None` = absent).
    pub before: Option<String>,
    /// Post-write on-disk value (the winner's value, never
    /// empty — empty / `None` winners short-circuit at the
    /// `Unmapped` branch).
    pub after: String,
}

/// Outcome of a single-file [`write_winners`] call.
#[derive(Debug, Clone, Default)]
pub struct WriteReport {
    /// Per-field before/after pairs for every winner that
    /// actually moved. `changes.len()` is the persisted-write
    /// count; the file gets `save_to_path`-d iff this vector is
    /// non-empty.
    pub changes: Vec<FieldChange>,
    /// How many winners matched what's already on disk (the
    /// dedup guard hit).
    pub fields_already_matched: usize,
    /// How many winners had a `Field` that the writer can't
    /// cover (today: `DurationSeconds`) OR for which the side-
    /// channel input wasn't available (e.g. `CoverUrl` with
    /// `cover_bytes == None`, or `Explicit` with an
    /// unclassifiable value) OR for which the file format
    /// doesn't support the custom tag (e.g. `Abridged` on a
    /// `FLAC` file — only `ID3v2` and `MP4` map cleanly). Logged so
    /// operators can track "how much of the data is reaching
    /// the file" coverage.
    pub fields_unmapped: usize,
}

impl WriteReport {
    /// Convenience accessor matching the prior shape — number
    /// of fields actually written.
    #[must_use]
    pub fn fields_changed(&self) -> usize {
        self.changes.len()
    }
}

/// Open `path` with lofty, set every supported winner that
/// differs from the current value, and save back.
///
/// `cover_bytes` is the pre-fetched cover-art payload (when
/// any). The stage calls [`ab_covers::CoverClient::fetch`]
/// once before this loop so multi-file books reuse the same
/// HTTP fetch. `None` means "no cover URL winner / fetch
/// failed / fetch disabled" — `Field::CoverUrl` then falls
/// through to `Unmapped`. C3a wires this in; C3b/C3c land the
/// remaining custom-tag fields + the shelf cover endpoint.
///
/// Returns a [`WriteReport`] with the per-field outcome counts.
/// `fields_changed == 0` means the file was not rewritten —
/// `save_to_path` is skipped entirely.
///
/// # Errors
///
/// - [`Error::Io`] if the file can't be opened / parsed by lofty
///   or if the save fails.
pub fn write_winners(
    path: &Path,
    winners: &[FieldWinner],
    cover_bytes: Option<&[u8]>,
) -> Result<WriteReport> {
    let mut report = WriteReport::default();

    // ── Phase 1: format-specific custom-tag dispatch ────────
    //
    // `Field::Abridged` can't be expressed through lofty's
    // abstract `Tag` API — it needs `Id3v2Tag::insert_user_text`
    // (TXXX:ABRIDGED) or `Ilst` with `AtomIdent::Freeform`
    // (`----:com.apple.iTunes:ABRIDGED`). The typed accessors
    // only exist on format-specific file types ([`MpegFile`] /
    // [`Mp4File`]), so this is a separate open via
    // `crate::abridged::write_abridged`.
    //
    // Done BEFORE the generic phase so the subsequent
    // `lofty::read_from_path` (Phase 2) picks up the new tag
    // and preserves it through the generic save — keeping the
    // write atomic from the report's perspective even though
    // two opens happened.
    apply_abridged_winners(path, winners, &mut report)?;

    // ── Phase 2: generic abstract-Tag winners ───────────────
    //
    // Everything else (Title, Author, Series, Language, Genre,
    // Publisher, Asin, Isbn, Subtitle, Description, ReleaseDate,
    // Narrator, CoverUrl, Explicit) goes through `apply_winner`
    // on the abstract `Tag`. Lofty translates `ItemKey` lookups
    // to the right format-specific frame under the hood.
    let any_non_abridged = winners.iter().any(|w| w.field != Field::Abridged);
    if !any_non_abridged {
        // Only Abridged in the winner set — Phase 1 already
        // saved if needed; skip the generic open entirely.
        return Ok(report);
    }

    let mut tagged = lofty::read_from_path(path).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "lofty open {}: {e}",
            path.display()
        )))
    })?;

    // Use the primary tag if present, else create a fresh one of
    // the file's preferred type. Lofty's primary-tag preference
    // matches the in-the-wild convention (ID3v2 for MP3, MP4
    // atoms for m4b/m4a). Two-step (peek without &mut, then
    // insert + take &mut) sidesteps the borrow-then-insert
    // conflict.
    if tagged.primary_tag().is_none() {
        let primary_type = tagged.primary_tag_type();
        tagged.insert_tag(Tag::new(primary_type));
    }
    let tag = tagged.primary_tag_mut().ok_or(Error::Invariant(
        "lofty primary_tag_mut returned None after insert_tag",
    ))?;

    // Track how many Phase 1 changes already landed so we can
    // detect whether Phase 2 added any NEW changes — that's
    // what gates the generic save.
    let phase1_changes = report.changes.len();

    for winner in winners.iter().filter(|w| w.field != Field::Abridged) {
        match apply_winner(tag, winner, cover_bytes) {
            FieldWriteOutcome::Changed { before } => {
                // For text-valued fields, `after` is the
                // winner's stringified value. For
                // `Field::CoverUrl` the on-disk artefact is
                // the picture blob; we record the source URL
                // so the audit log carries something human-
                // readable rather than embedding base64
                // bytes.
                let after = winner.value.clone().unwrap_or_default();
                report.changes.push(FieldChange {
                    field: winner.field,
                    before,
                    after,
                });
            }
            FieldWriteOutcome::Matched => report.fields_already_matched += 1,
            FieldWriteOutcome::Unmapped => report.fields_unmapped += 1,
        }
    }

    // Skip the generic save if Phase 2 added no changes (a Phase
    // 1 abridged save may have happened — that's already on
    // disk).
    let phase2_added = report.changes.len() > phase1_changes;
    if !phase2_added {
        return Ok(report);
    }

    tagged
        .save_to_path(path, WriteOptions::default())
        .map_err(|e| {
            Error::Io(std::io::Error::other(format!(
                "lofty save {}: {e}",
                path.display()
            )))
        })?;

    Ok(report)
}

/// Apply every `Field::Abridged` winner in `winners`, updating
/// `report`. Iterates because in principle the winners list
/// could carry multiple Abridged rows (e.g. multi-source
/// consensus); in practice it's at most one. The function still
/// handles N>1 gracefully — last writer wins, every prior write
/// counts as `Changed` against the previous on-disk state.
fn apply_abridged_winners(
    path: &Path,
    winners: &[FieldWinner],
    report: &mut WriteReport,
) -> Result<()> {
    for winner in winners.iter().filter(|w| w.field == Field::Abridged) {
        let Some(value) = winner.value.as_deref() else {
            // Same convention as `apply_winner` — `None` value
            // is not "delete the tag" but "no mapping today".
            report.fields_unmapped += 1;
            continue;
        };
        if value.is_empty() {
            report.fields_unmapped += 1;
            continue;
        }
        match crate::abridged::write_abridged(path, value)? {
            FieldWriteOutcome::Changed { before } => {
                report.changes.push(FieldChange {
                    field: Field::Abridged,
                    before,
                    after: value.to_owned(),
                });
            }
            FieldWriteOutcome::Matched => report.fields_already_matched += 1,
            FieldWriteOutcome::Unmapped => report.fields_unmapped += 1,
        }
    }
    Ok(())
}

/// Per-field outcome — used by [`write_winners`] to bucket
/// each winner. Private; the public surface is the aggregate
/// [`WriteReport`].
///
/// `Changed` carries the pre-write value so the per-file
/// audit-log writer can record `(before, after)` without a
/// second pass over the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldWriteOutcome {
    /// On-disk value was missing or differed; we wrote the
    /// winner. `before` is the pre-mutation value (or `None`
    /// if the tag was previously absent).
    Changed { before: Option<String> },
    /// On-disk value already equals the winner (skip-write).
    Matched,
    /// No mapping yet for this `Field` variant.
    Unmapped,
}

/// Apply one winner to the in-memory tag. Returns whether the
/// tag actually moved, was a no-op, or had no mapping.
///
/// `None`-valued winners (the row exists in
/// `book_field_provenance` but `value IS NULL`) are treated as
/// `Unmapped` — we don't write empty strings nor remove the
/// tag in this slice (ADR-0028 didn't pick a "winner=NULL means
/// remove" interpretation).
///
/// `cover_bytes` is consulted only on the `Field::CoverUrl`
/// branch; every other field ignores it. When `CoverUrl`
/// arrives but `cover_bytes` is `None` (no successful HTTP
/// fetch upstream), the field falls through to `Unmapped` so
/// the audit log reflects "we tried but didn't write."
fn apply_winner(
    tag: &mut Tag,
    winner: &FieldWinner,
    cover_bytes: Option<&[u8]>,
) -> FieldWriteOutcome {
    let Some(new_value) = winner.value.as_deref() else {
        return FieldWriteOutcome::Unmapped;
    };
    if new_value.is_empty() {
        return FieldWriteOutcome::Unmapped;
    }

    // Two flavours of lofty access:
    //
    //  * The 3 typed accessors (`title`, `artist`, `album`) own
    //    convenience setters that handle the underlying
    //    `ItemKey` choice per tag format.
    //  * The rest use `get_string` + `insert_text` on a typed
    //    `ItemKey`. ItemKey enumerates the standard keys; lofty
    //    maps to the right format-specific frame (TPUB / ©pub /
    //    PUBLISHER) under the hood.
    match winner.field {
        Field::Title => {
            let before = tag.title().as_deref().map(str::to_owned);
            if before.as_deref() == Some(new_value) {
                FieldWriteOutcome::Matched
            } else {
                tag.set_title(new_value.to_owned());
                FieldWriteOutcome::Changed { before }
            }
        }
        Field::Author => {
            let before = tag.artist().as_deref().map(str::to_owned);
            if before.as_deref() == Some(new_value) {
                FieldWriteOutcome::Matched
            } else {
                tag.set_artist(new_value.to_owned());
                FieldWriteOutcome::Changed { before }
            }
        }
        Field::Series => {
            let before = tag.album().as_deref().map(str::to_owned);
            if before.as_deref() == Some(new_value) {
                FieldWriteOutcome::Matched
            } else {
                tag.set_album(new_value.to_owned());
                FieldWriteOutcome::Changed { before }
            }
        }
        Field::Language => set_item_if_changed(tag, ItemKey::Language, new_value),
        Field::Genre => set_item_if_changed(tag, ItemKey::Genre, new_value),
        Field::Publisher => set_item_if_changed(tag, ItemKey::Publisher, new_value),
        Field::Asin => set_item_if_changed(tag, ItemKey::CatalogNumber, new_value),
        Field::Isbn => set_item_if_changed(tag, ItemKey::Isrc, new_value),

        // Slice 2 of the field-mapping expansion. Lofty's
        // `ItemKey` enumerates the standard tag keys; each
        // here lands the field on the format-specific frame
        // (ID3 `TIT3` / `COMM` / `TDRC` / `TCOM`; the MP4
        // and Vorbis equivalents) without per-format
        // branching.
        Field::Subtitle => set_item_if_changed(tag, ItemKey::TrackSubtitle, new_value),
        // Audiobook description / synopsis lands in the
        // generic Comment frame. ID3v2 `COMM`, MP4 `©cmt`,
        // Vorbis `COMMENT` — same logical home everywhere.
        Field::Description => set_item_if_changed(tag, ItemKey::Comment, new_value),
        // `RecordingDate` (ID3 `TDRC` / "Year") is the
        // ecosystem's de-facto release-date frame even when
        // the value is a full ISO-8601 date string —
        // lofty's `ItemKey` docs call this out explicitly
        // ("Year" used even for full date strings).
        Field::ReleaseDate => set_item_if_changed(tag, ItemKey::RecordingDate, new_value),
        // Audiobook convention: narrator goes in the
        // Composer frame (ID3 `TCOM`, MP4 `©wrt`). Both
        // ABS and the older ABtagger workflow round-trip
        // through this key.
        Field::Narrator => set_item_if_changed(tag, ItemKey::Composer, new_value),

        Field::CoverUrl => apply_cover(tag, cover_bytes),

        // Slice C3b: Explicit lands on `ItemKey::ParentalAdvisory`
        // which lofty maps to ID3v2 `ITUNESADVISORY` (`TXXX`
        // frame with that user-defined description) and to the
        // MP4 `rtng` atom — the convention every Apple-adjacent
        // ecosystem reads. iTunes' integer value scheme is
        // `0=None`, `1=Clean`, `2=Explicit (old)`,
        // `4=Explicit`. We normalise the catalog's truthy/falsy
        // string into that integer below.
        Field::Explicit => apply_explicit(tag, new_value),

        // Two distinct reasons for `Unmapped`:
        //
        // - `Field::Abridged`: handled by `apply_abridged_winners`
        //   (Phase 1) before this loop runs — it requires
        //   format-specific tag dispatch via `crate::abridged`.
        //   Returning `Unmapped` here would double-count it;
        //   the abstract loop filters it out at the call site,
        //   so this branch is a defensive fall-through.
        // - `Field::DurationSeconds`: typically derived from
        //   the audio decode, not a separate tag frame. The
        //   `book_field_provenance.duration_seconds` row
        //   carries the consensus winner but a future
        //   "duration-as-tag" surface needs design work.
        //
        // `match_same_arms` would collapse these but the two
        // reasons are semantically distinct — keeping them
        // apart aids the reader.
        #[allow(
            clippy::match_same_arms,
            reason = "distinct semantic reasons for Unmapped"
        )]
        Field::Abridged => FieldWriteOutcome::Unmapped,
        #[allow(
            clippy::match_same_arms,
            reason = "distinct semantic reasons for Unmapped"
        )]
        Field::DurationSeconds => FieldWriteOutcome::Unmapped,
    }
}

/// Embed a fetched cover blob as the file's front-cover
/// [`Picture`].
///
/// Detects the image's MIME type from its first few bytes (PNG
/// magic, JPEG SOI, GIF / `WebP` / AVIF signatures) and
/// constructs a [`PictureType::CoverFront`] picture via
/// `Picture::new_unchecked` — the call is "unchecked" because
/// lofty's standard `Picture::from_reader` requires a typed
/// `Cursor` and our caller already has a `&[u8]`; the MIME
/// sniff below restores the validation lofty would have done.
///
/// **Dedup**: when a cover with the same byte content is
/// already on the tag, returns `Matched`. Byte-equal is the
/// strictest guard; a future revision could compare hashes if
/// large images make the equality check expensive.
fn apply_cover(tag: &mut Tag, cover_bytes: Option<&[u8]>) -> FieldWriteOutcome {
    let Some(bytes) = cover_bytes else {
        return FieldWriteOutcome::Unmapped;
    };
    if bytes.is_empty() {
        return FieldWriteOutcome::Unmapped;
    }
    let Some(mime) = sniff_image_mime(bytes) else {
        // We refuse to embed bytes we can't classify — the
        // file's tag would then reference an opaque blob.
        return FieldWriteOutcome::Unmapped;
    };

    // Dedup: if any existing picture has identical bytes, skip.
    // We don't restrict to `CoverFront` because some files
    // carry cover art under other type codes (icon, other) and
    // we'd rather not duplicate.
    let existing = tag.pictures();
    if existing.iter().any(|p| p.data() == bytes) {
        return FieldWriteOutcome::Matched;
    }

    // lofty 0.24's `Picture` is constructed via a builder
    // (`Picture::unchecked` for raw bytes that bypass the
    // MIME-from-content sniff; we've already done our own sniff
    // above and the test fixtures exercise both paths).
    let picture = Picture::unchecked(bytes.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(mime)
        .description("Cover")
        .build();

    // Lofty's set_picture replaces the picture at the given
    // index; for a fresh tag the index doesn't exist yet, so
    // use `push_picture` which always appends. The audit log
    // records "we wrote a cover" rather than the bytes.
    tag.push_picture(picture);
    FieldWriteOutcome::Changed { before: None }
}

/// Best-effort image MIME-type sniff from leading bytes. Covers
/// PNG / JPEG / GIF / `WebP` / AVIF / BMP — the formats Audnexus
/// / Audible CDNs serve. Unknown payloads return `None` so the
/// caller refuses to embed them.
fn sniff_image_mime(bytes: &[u8]) -> Option<MimeType> {
    if bytes.len() >= 8 && &bytes[0..8] == b"\x89PNG\r\n\x1a\n" {
        return Some(MimeType::Png);
    }
    if bytes.len() >= 3 && &bytes[0..3] == b"\xFF\xD8\xFF" {
        return Some(MimeType::Jpeg);
    }
    if bytes.len() >= 6 && (&bytes[0..6] == b"GIF87a" || &bytes[0..6] == b"GIF89a") {
        return Some(MimeType::Gif);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        // Lofty's `MimeType` enum doesn't yet carry a `Webp`
        // variant; tag formats that accept arbitrary MIME
        // strings can still take WebP, but the `MimeType`
        // round-trip would lose the precision. Treat WebP
        // as `Unknown("image/webp")` so callers see the
        // round-trip explicitly.
        return Some(MimeType::Unknown("image/webp".to_owned()));
    }
    if bytes.len() >= 12 && &bytes[4..12] == b"ftypavif" {
        return Some(MimeType::Unknown("image/avif".to_owned()));
    }
    if bytes.len() >= 2 && &bytes[0..2] == b"BM" {
        return Some(MimeType::Bmp);
    }
    None
}

/// `ItemKey`-based set with a dedup guard. Captures the
/// pre-write value (None if absent) for the audit log.
fn set_item_if_changed(tag: &mut Tag, key: ItemKey, new_value: &str) -> FieldWriteOutcome {
    let before = tag.get_string(key).map(str::to_owned);
    if before.as_deref().map(str::trim) == Some(new_value) {
        return FieldWriteOutcome::Matched;
    }
    tag.insert_text(key, new_value.to_owned());
    FieldWriteOutcome::Changed { before }
}

/// Translate the catalog's truthy / falsy string into the
/// iTunes advisory integer + dispatch through
/// [`ItemKey::ParentalAdvisory`]. Returns `Unmapped` for
/// inputs we can't classify (rather than guessing — the
/// audit log surfaces "we didn't write" so an operator can
/// fix the source).
fn apply_explicit(tag: &mut Tag, new_value: &str) -> FieldWriteOutcome {
    let Some(int_str) = explicit_value_to_advisory(new_value) else {
        return FieldWriteOutcome::Unmapped;
    };
    set_item_if_changed(tag, ItemKey::ParentalAdvisory, int_str)
}

/// Map a catalog truthy / falsy string to the iTunes advisory
/// integer as a `&'static str`. Returns `None` when the input
/// can't be classified (preserves the audit-log "Unmapped"
/// signal).
///
/// Apple's `rtng` atom values:
/// - `0` — None (not rated)
/// - `1` — Clean
/// - `2` — Explicit (deprecated, kept for back-compat)
/// - `4` — Explicit (modern)
///
/// We emit `"4"` for truthy + `"0"` for falsy. `Clean` (1)
/// isn't reachable from the boolean source; future schema
/// could promote to a tri-state.
fn explicit_value_to_advisory(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        // Truthy: explicit content acknowledged.
        "1" | "true" | "t" | "yes" | "y" | "explicit" => Some("4"),
        // Falsy: not flagged.
        "0" | "false" | "f" | "no" | "n" | "none" | "clean" => Some("0"),
        // Unknown / not a boolean — leave the tag alone.
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ab_core::Field;

    #[test]
    fn still_unmapped_fields_return_unmapped_outcome() {
        // After slice #98 (abridged custom-tag dispatch), only
        // `DurationSeconds` returns `Unmapped` from `apply_winner`.
        // `Abridged` also returns `Unmapped` from this helper
        // because the abstract-Tag path can't write it — but the
        // public `write_winners` entry point routes it through
        // `apply_abridged_winners` (Phase 1) before reaching this
        // loop. See `apply_winner` doc comment for the contract.
        //
        // CoverUrl + Explicit have their own dedicated tests
        // below.
        let v: Vec<(Field, FieldWriteOutcome)> = [Field::DurationSeconds, Field::Abridged]
            .iter()
            .map(|f| {
                let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
                let w = FieldWinner {
                    field: *f,
                    value: Some("anything".to_owned()),
                    source: "any".to_owned(),
                };
                (*f, apply_winner(&mut tag, &w, None))
            })
            .collect();
        for (field, outcome) in v {
            assert_eq!(
                outcome,
                FieldWriteOutcome::Unmapped,
                "{field:?} should be unmapped in this slice"
            );
        }
    }

    #[test]
    fn explicit_truthy_writes_advisory_4() {
        for value in ["1", "true", "TRUE", "yes", "Y", "explicit"] {
            let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
            let w = FieldWinner {
                field: Field::Explicit,
                value: Some(value.to_owned()),
                source: "enrich-from-audnexus".to_owned(),
            };
            assert!(
                matches!(
                    apply_winner(&mut tag, &w, None),
                    FieldWriteOutcome::Changed { before: None }
                ),
                "{value:?} should map to advisory=4 / Changed"
            );
            assert_eq!(
                tag.get_string(ItemKey::ParentalAdvisory),
                Some("4"),
                "iTunes advisory integer for Explicit"
            );
        }
    }

    #[test]
    fn explicit_falsy_writes_advisory_0() {
        for value in ["0", "false", "no", "n", "clean", "none"] {
            let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
            let w = FieldWinner {
                field: Field::Explicit,
                value: Some(value.to_owned()),
                source: "enrich-from-audnexus".to_owned(),
            };
            assert!(
                matches!(
                    apply_winner(&mut tag, &w, None),
                    FieldWriteOutcome::Changed { before: None }
                ),
                "{value:?} should map to advisory=0 / Changed"
            );
            assert_eq!(
                tag.get_string(ItemKey::ParentalAdvisory),
                Some("0"),
                "iTunes advisory integer for Not Flagged"
            );
        }
    }

    #[test]
    fn explicit_unclassifiable_value_is_unmapped() {
        // The catalog could theoretically emit a stray value
        // we don't recognise; rather than guess + write the
        // wrong integer we leave the tag alone.
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        let w = FieldWinner {
            field: Field::Explicit,
            value: Some("kinda?".to_owned()),
            source: "enrich-from-audnexus".to_owned(),
        };
        assert_eq!(
            apply_winner(&mut tag, &w, None),
            FieldWriteOutcome::Unmapped,
            "unrecognised truthy → Unmapped"
        );
        assert_eq!(
            tag.get_string(ItemKey::ParentalAdvisory),
            None,
            "tag untouched"
        );
    }

    #[test]
    fn explicit_dedup_matches_when_advisory_already_set() {
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        tag.insert_text(ItemKey::ParentalAdvisory, "4".to_owned());
        let w = FieldWinner {
            field: Field::Explicit,
            value: Some("true".to_owned()),
            source: "enrich-from-audnexus".to_owned(),
        };
        assert_eq!(
            apply_winner(&mut tag, &w, None),
            FieldWriteOutcome::Matched,
            "advisory already = 4 → Matched (no re-write)"
        );
    }

    #[test]
    fn explicit_value_to_advisory_matrix() {
        // Pin the truthy / falsy alphabets so a future tweak
        // (or a CHECK constraint on the catalog's value
        // column) doesn't drift silently.
        assert_eq!(explicit_value_to_advisory("true"), Some("4"));
        assert_eq!(explicit_value_to_advisory("FALSE"), Some("0"));
        assert_eq!(explicit_value_to_advisory("  Yes "), Some("4"));
        assert_eq!(explicit_value_to_advisory("clean"), Some("0"));
        assert_eq!(explicit_value_to_advisory("maybe"), None);
        assert_eq!(explicit_value_to_advisory(""), None);
    }

    #[test]
    fn cover_url_without_bytes_is_unmapped() {
        // The stage's `run()` is responsible for fetching
        // the bytes BEFORE calling `write_winners`; if the
        // fetch failed, `cover_bytes` is `None` and the
        // CoverUrl winner falls through to Unmapped.
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        let w = FieldWinner {
            field: Field::CoverUrl,
            value: Some("https://example.invalid/cover.jpg".to_owned()),
            source: "enrich-from-audnexus".to_owned(),
        };
        assert_eq!(
            apply_winner(&mut tag, &w, None),
            FieldWriteOutcome::Unmapped,
            "no bytes → Unmapped"
        );
    }

    #[test]
    fn cover_url_with_bytes_embeds_a_front_cover_picture() {
        // Minimal-but-valid PNG magic + IHDR. Lofty's tag
        // doesn't validate the image past the bytes-equal
        // dedup; the MIME sniff is what enforces format.
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR_rest_ignored".to_vec();
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        let w = FieldWinner {
            field: Field::CoverUrl,
            value: Some("https://example.invalid/cover.png".to_owned()),
            source: "enrich-from-audnexus".to_owned(),
        };
        let outcome = apply_winner(&mut tag, &w, Some(&png));
        assert!(
            matches!(outcome, FieldWriteOutcome::Changed { before: None }),
            "got {outcome:?}"
        );
        let pictures = tag.pictures();
        assert_eq!(pictures.len(), 1, "exactly one picture appended");
        assert_eq!(pictures[0].pic_type(), PictureType::CoverFront);
        assert_eq!(pictures[0].data(), &png[..]);
        assert_eq!(pictures[0].mime_type(), Some(&MimeType::Png));
    }

    #[test]
    fn cover_url_dedups_when_existing_picture_matches() {
        let jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF".to_vec();
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        tag.push_picture(
            Picture::unchecked(jpeg.clone())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );
        let w = FieldWinner {
            field: Field::CoverUrl,
            value: Some("https://example.invalid/cover.jpg".to_owned()),
            source: "enrich-from-audnexus".to_owned(),
        };
        let outcome = apply_winner(&mut tag, &w, Some(&jpeg));
        assert_eq!(
            outcome,
            FieldWriteOutcome::Matched,
            "byte-equal existing → Matched"
        );
        assert_eq!(tag.pictures().len(), 1, "no duplicate appended");
    }

    #[test]
    fn cover_url_with_unsniffable_bytes_is_unmapped() {
        // Random bytes don't match any known image signature.
        // Refuse to embed — we'd otherwise put an opaque blob
        // in the tag with a guessed MIME type.
        let garbage = b"not an image".to_vec();
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        let w = FieldWinner {
            field: Field::CoverUrl,
            value: Some("https://example.invalid/cover".to_owned()),
            source: "enrich-from-audnexus".to_owned(),
        };
        assert_eq!(
            apply_winner(&mut tag, &w, Some(&garbage)),
            FieldWriteOutcome::Unmapped,
            "unsniffable → Unmapped (refuse to embed unknown bytes)"
        );
    }

    #[test]
    fn missing_or_empty_value_is_unmapped() {
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        let none = FieldWinner {
            field: Field::Title,
            value: None,
            source: "any".to_owned(),
        };
        assert_eq!(
            apply_winner(&mut tag, &none, None),
            FieldWriteOutcome::Unmapped
        );
        let empty = FieldWinner {
            field: Field::Title,
            value: Some(String::new()),
            source: "any".to_owned(),
        };
        assert_eq!(
            apply_winner(&mut tag, &empty, None),
            FieldWriteOutcome::Unmapped
        );
    }

    #[test]
    fn newly_mapped_fields_write_to_expected_item_keys() {
        // Pin the four mappings added in slice 2 (Subtitle,
        // Description, ReleaseDate, Narrator). If a future
        // lofty bump renames any of these ItemKeys this test
        // turns red before the runtime hit.
        let cases: &[(Field, ItemKey, &str)] = &[
            (Field::Subtitle, ItemKey::TrackSubtitle, "Vol. 1"),
            (Field::Description, ItemKey::Comment, "An adventure tale"),
            (Field::ReleaseDate, ItemKey::RecordingDate, "1951-06-01"),
            (Field::Narrator, ItemKey::Composer, "Scott Brick"),
        ];
        for (field, expected_key, value) in cases {
            let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
            let w = FieldWinner {
                field: *field,
                value: Some((*value).to_owned()),
                source: "enrich-from-audnexus".to_owned(),
            };
            // Newly-mapped fields: before is None (Tag::new starts empty)
            // so we expect `Changed { before: None }`.
            assert!(
                matches!(
                    apply_winner(&mut tag, &w, None),
                    FieldWriteOutcome::Changed { before: None }
                ),
                "{field:?} should write with before=None"
            );
            assert_eq!(
                tag.get_string(*expected_key),
                Some(*value),
                "{field:?} should land on {expected_key:?}"
            );
        }
    }

    #[test]
    fn item_key_dedup_matches_on_trimmed_value() {
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        tag.insert_text(ItemKey::Publisher, "Audible".to_owned());
        let same = FieldWinner {
            field: Field::Publisher,
            value: Some("Audible".to_owned()),
            source: "audnexus".to_owned(),
        };
        assert_eq!(
            apply_winner(&mut tag, &same, None),
            FieldWriteOutcome::Matched
        );
    }

    #[test]
    fn item_key_changed_on_different_value() {
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        tag.insert_text(ItemKey::Publisher, "Audible".to_owned());
        let different = FieldWinner {
            field: Field::Publisher,
            value: Some("Penguin Audio".to_owned()),
            source: "audnexus".to_owned(),
        };
        // Different from current "Audible" → before captures
        // the prior value for the audit log.
        let outcome = apply_winner(&mut tag, &different, None);
        assert!(
            matches!(
                outcome,
                FieldWriteOutcome::Changed { before: Some(ref b) } if b == "Audible"
            ),
            "expected Changed {{ before = Some(\"Audible\") }}, got {outcome:?}",
        );
        assert_eq!(tag.get_string(ItemKey::Publisher), Some("Penguin Audio"));
    }

    #[test]
    fn sniff_image_mime_classifies_common_formats() {
        assert_eq!(
            sniff_image_mime(b"\x89PNG\r\n\x1a\n\x00\x00\x00\r"),
            Some(MimeType::Png)
        );
        assert_eq!(
            sniff_image_mime(b"\xFF\xD8\xFFsuffix"),
            Some(MimeType::Jpeg)
        );
        assert_eq!(sniff_image_mime(b"GIF89a..."), Some(MimeType::Gif));
        assert_eq!(sniff_image_mime(b"BMpixels..."), Some(MimeType::Bmp));
        assert_eq!(sniff_image_mime(b"too short"), None);
        assert_eq!(sniff_image_mime(b"nothing recognizable"), None);
    }
}
