//! `lofty`-based per-file tag-write helpers.
//!
//! The stage's `run()` decides *what* to write (winners from
//! `book_field_provenance`); this module decides *how* to map
//! those typed winners onto lofty `Tag` items and persist them.
//!
//! ## Field coverage
//!
//! 12 of 16 `book_field_provenance` fields map to a canonical
//! lofty key. Symmetric with [`ab_tag_read`]'s read path:
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
//!
//! The remaining 4 fields are deliberately unmapped:
//!
//! - `DurationSeconds` — derived from decode, not a tag frame.
//! - `CoverUrl` — cover art is a `Picture` blob; needs a
//!   fetch-then-embed slice.
//! - `Abridged` / `Explicit` — no canonical standard tag;
//!   custom-key handling slated for a follow-up.
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
use lofty::tag::{Accessor, ItemKey, Tag};
use std::path::Path;

use crate::winners::FieldWinner;

/// Outcome of a single-file [`write_winners`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteReport {
    /// How many winner values were actually persisted (i.e.
    /// differed from on-disk).
    pub fields_changed: usize,
    /// How many winners matched what's already on disk (the
    /// dedup guard hit).
    pub fields_already_matched: usize,
    /// How many winners had a `Field` that the slice's mapping
    /// doesn't cover yet (Subtitle, Description, …). Logged so
    /// operators can track "how much of the data is reaching
    /// the file" coverage.
    pub fields_unmapped: usize,
}

/// Open `path` with lofty, set every supported winner that
/// differs from the current value, and save back.
///
/// Returns a [`WriteReport`] with the per-field outcome counts.
/// `fields_changed == 0` means the file was not rewritten —
/// `save_to_path` is skipped entirely.
///
/// # Errors
///
/// - [`Error::Io`] if the file can't be opened / parsed by lofty
///   or if the save fails.
pub fn write_winners(path: &Path, winners: &[FieldWinner]) -> Result<WriteReport> {
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

    let mut report = WriteReport::default();
    for winner in winners {
        match apply_winner(tag, winner) {
            FieldWriteOutcome::Changed => report.fields_changed += 1,
            FieldWriteOutcome::Matched => report.fields_already_matched += 1,
            FieldWriteOutcome::Unmapped => report.fields_unmapped += 1,
        }
    }

    if report.fields_changed == 0 {
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

/// Per-field outcome — used by [`write_winners`] to bucket
/// each winner. Private; the public surface is the aggregate
/// [`WriteReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldWriteOutcome {
    /// On-disk value was missing or differed; we wrote the
    /// winner.
    Changed,
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
fn apply_winner(tag: &mut Tag, winner: &FieldWinner) -> FieldWriteOutcome {
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
            if tag.title().as_deref() == Some(new_value) {
                FieldWriteOutcome::Matched
            } else {
                tag.set_title(new_value.to_owned());
                FieldWriteOutcome::Changed
            }
        }
        Field::Author => {
            if tag.artist().as_deref() == Some(new_value) {
                FieldWriteOutcome::Matched
            } else {
                tag.set_artist(new_value.to_owned());
                FieldWriteOutcome::Changed
            }
        }
        Field::Series => {
            if tag.album().as_deref() == Some(new_value) {
                FieldWriteOutcome::Matched
            } else {
                tag.set_album(new_value.to_owned());
                FieldWriteOutcome::Changed
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

        // Remaining unmapped fields:
        //
        // - `DurationSeconds` — typically derived from the
        //   audio decode, not a separate tag frame. The
        //   `book_field_provenance.duration_seconds` row
        //   carries the consensus winner but a future
        //   "duration-as-tag" surface needs design work.
        // - `CoverUrl` — cover art is a `Picture` blob, not
        //   a string; needs a fetch-then-embed slice.
        // - `Abridged` / `Explicit` — no canonical standard
        //   tag; usually custom keys per encoder. Skipped
        //   until the convention question is decided.
        Field::DurationSeconds | Field::CoverUrl | Field::Abridged | Field::Explicit => {
            FieldWriteOutcome::Unmapped
        }
    }
}

/// `ItemKey`-based set with a dedup guard.
fn set_item_if_changed(tag: &mut Tag, key: ItemKey, new_value: &str) -> FieldWriteOutcome {
    if tag.get_string(key).map(str::trim) == Some(new_value) {
        return FieldWriteOutcome::Matched;
    }
    tag.insert_text(key, new_value.to_owned());
    FieldWriteOutcome::Changed
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use ab_core::Field;

    #[test]
    fn unmapped_fields_return_unmapped_outcome() {
        // The 4 still-unmapped variants from the table in the
        // module doc all return Unmapped today; pin so the
        // field-set doesn't drift without a docstring update.
        // (Slice 1 listed 8 unmapped; slice 2 added Subtitle,
        // Description, ReleaseDate, Narrator — now 4.)
        let v: Vec<(Field, FieldWriteOutcome)> = [
            Field::DurationSeconds,
            Field::CoverUrl,
            Field::Abridged,
            Field::Explicit,
        ]
        .iter()
        .map(|f| {
            let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
            let w = FieldWinner {
                field: *f,
                value: Some("anything".to_owned()),
                source: "any".to_owned(),
            };
            (*f, apply_winner(&mut tag, &w))
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
    fn missing_or_empty_value_is_unmapped() {
        let mut tag = Tag::new(lofty::tag::TagType::Id3v2);
        let none = FieldWinner {
            field: Field::Title,
            value: None,
            source: "any".to_owned(),
        };
        assert_eq!(apply_winner(&mut tag, &none), FieldWriteOutcome::Unmapped);
        let empty = FieldWinner {
            field: Field::Title,
            value: Some(String::new()),
            source: "any".to_owned(),
        };
        assert_eq!(apply_winner(&mut tag, &empty), FieldWriteOutcome::Unmapped);
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
                source: "audnexus-enrich".to_owned(),
            };
            assert_eq!(
                apply_winner(&mut tag, &w),
                FieldWriteOutcome::Changed,
                "{field:?} should write"
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
        assert_eq!(apply_winner(&mut tag, &same), FieldWriteOutcome::Matched);
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
        assert_eq!(
            apply_winner(&mut tag, &different),
            FieldWriteOutcome::Changed
        );
        assert_eq!(tag.get_string(ItemKey::Publisher), Some("Penguin Audio"));
    }
}
