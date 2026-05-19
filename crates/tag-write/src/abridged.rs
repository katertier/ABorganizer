//! Format-specific writer for `Field::Abridged`.
//!
//! The abstract [`lofty::tag::Tag`] interface has no `ItemKey`
//! variant for "abridged audiobook" — it's a long-tail audiobook-
//! specific flag, not part of the ID3 / Vorbis / Apple-iTunes
//! core vocabulary. Writing it requires dropping below the
//! abstract `Tag` API to format-specific types:
//!
//! - **`ID3v2`** (MP3 + AAC + WAV + AIFF): the iTunes convention
//!   is a `TXXX` "user-defined text" frame with description
//!   `ABRIDGED` and value `"true"` / `"false"`. Lofty exposes
//!   this via `Id3v2Tag::insert_user_text` /
//!   `Id3v2Tag::get_user_text`.
//! - **MP4** (m4a + m4b): the iTunes convention is a freeform
//!   atom `----:com.apple.iTunes:ABRIDGED`. Lofty's `Ilst`
//!   stores this as an [`Atom`] with
//!   [`AtomIdent::Freeform`] and `AtomData::UTF8`.
//!
//! Other formats (`FLAC`, Vorbis, Opus, APE, Speex, `WavPack`)
//! return [`FieldWriteOutcome::Unmapped`]. They have their own
//! comment-style tag systems that could in theory carry an
//! `ABRIDGED` field, but the user base for audiobooks in those
//! formats is small enough that the complexity isn't worth it
//! today.
//!
//! # Why a separate file open
//!
//! `write_winners` already opens the file via
//! [`lofty::read_from_path`] which yields a generic `TaggedFile`.
//! The typed `Id3v2Tag` / `Ilst` accessors are only available on
//! format-specific file types ([`MpegFile`] / [`Mp4File`]); the
//! generic `TaggedFile` only exposes `&mut Tag`. So a typed
//! write requires a separate open via the typed loader.
//!
//! The cost is a second read+save for books that have an
//! abridged winner. In practice this is a small fraction of
//! books (the field is rare in catalog metadata; the main
//! producer is `PATCH /api/v1/books/{id}` user edits). For books
//! with NO abridged winner, the typed path is skipped entirely
//! by the caller — zero overhead.

use std::borrow::Cow;
use std::fs::OpenOptions;
use std::io::{Seek as _, SeekFrom};
use std::path::Path;

use ab_core::{Error, Result};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType};
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::probe::Probe;

use crate::ITUNES_MEAN;
use crate::write::FieldWriteOutcome;

/// User-defined text frame description (`ID3v2`) / freeform name
/// suffix (MP4). Single shared constant so the two format
/// branches can't drift on capitalization (`ABRIDGED` vs
/// `Abridged` would each create a different on-disk tag).
const ABRIDGED_TAG_NAME: &str = "ABRIDGED";

/// Write the `Abridged` field as a format-specific custom tag.
///
/// Probes the file type, dispatches to the typed writer, and
/// returns the outcome:
///
/// - [`FieldWriteOutcome::Changed`] — file rewritten with the
///   new value. `before` is the pre-mutation on-disk value (or
///   `None` if the tag was absent).
/// - [`FieldWriteOutcome::Matched`] — on-disk value already
///   equals `value`; file not rewritten.
/// - [`FieldWriteOutcome::Unmapped`] — file format isn't
///   `ID3v2` or `MP4` (or the probe failed). No write
///   attempted.
///
/// # Errors
///
/// - [`Error::Io`] if the file can't be opened, lofty's typed
///   reader fails, or the typed save fails.
#[allow(
    unreachable_pub,
    reason = "callable from sibling crate modules; the parent module is pub(crate) which makes this `pub` effectively pub(crate), but writing pub(crate) here triggers clippy::redundant_pub_crate — pick one consistently"
)]
pub fn write_abridged(path: &Path, value: &str) -> Result<FieldWriteOutcome> {
    // Probe first to dispatch by format. `guess_file_type()` is
    // a lightweight header sniff, not a full parse — cheap to
    // run for the "abridged not present" early-exit case.
    let probe = Probe::open(path)
        .map_err(|e| Error::Io(std::io::Error::other(format!("abridged probe open: {e}"))))?;
    let probe = probe.guess_file_type().map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "abridged guess_file_type: {e}"
        )))
    })?;

    match probe.file_type() {
        Some(FileType::Mpeg | FileType::Aac | FileType::Wav | FileType::Aiff) => {
            write_abridged_id3v2(path, value)
        }
        Some(FileType::Mp4) => write_abridged_mp4(path, value),
        // FLAC / Vorbis / Opus / APE / Speex / WavPack / MPC /
        // Custom: no ABRIDGED convention; return Unmapped so the
        // report's `fields_unmapped` counter reflects the gap.
        _ => Ok(FieldWriteOutcome::Unmapped),
    }
}

/// `ID3v2` TXXX:ABRIDGED writer. Used for MP3
/// (`FileType::Mpeg`) plus the rarer AAC / WAV / AIFF cases
/// where `ID3v2` is the primary tag format.
fn write_abridged_id3v2(path: &Path, value: &str) -> Result<FieldWriteOutcome> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(Error::Io)?;
    let mut mpeg = <MpegFile as AudioFile>::read_from(&mut file, ParseOptions::default())
        .map_err(|e| Error::Io(std::io::Error::other(format!("abridged id3 read: {e}"))))?;

    // Take the existing tag by value so we can mutate without
    // overlapping borrows of `mpeg`. `std::mem::take` leaves
    // `None` in place; we put the mutated tag back via
    // `set_id3v2` before saving.
    let mut id3v2 = mpeg.id3v2_mut().map(std::mem::take).unwrap_or_default();

    let before = id3v2.get_user_text(ABRIDGED_TAG_NAME).map(str::to_owned);
    if before.as_deref() == Some(value) {
        // Idempotence: skip the save when on-disk already
        // matches. The `mpeg.set_id3v2(id3v2)` call below would
        // be a no-op anyway, but skipping save_to avoids the
        // file rewrite (and the mtime bump).
        return Ok(FieldWriteOutcome::Matched);
    }

    let _ = id3v2.insert_user_text(ABRIDGED_TAG_NAME.to_owned(), value.to_owned());
    let _ = mpeg.set_id3v2(id3v2);

    file.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    mpeg.save_to(&mut file, WriteOptions::default())
        .map_err(|e| Error::Io(std::io::Error::other(format!("abridged id3 save: {e}"))))?;

    Ok(FieldWriteOutcome::Changed { before })
}

/// MP4 freeform atom writer. Used for m4a / m4b (`FileType::Mp4`).
fn write_abridged_mp4(path: &Path, value: &str) -> Result<FieldWriteOutcome> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(Error::Io)?;
    let mut mp4 = <Mp4File as AudioFile>::read_from(&mut file, ParseOptions::default())
        .map_err(|e| Error::Io(std::io::Error::other(format!("abridged mp4 read: {e}"))))?;

    let mut ilst: Ilst = mp4.ilst_mut().map(std::mem::take).unwrap_or_default();

    // Build the freeform identifier once and reuse for both the
    // get + insert. Cow::Borrowed avoids allocating for the
    // read-only get; the insert clones to 'static lifetime
    // inside `Atom::new`.
    let ident_borrowed = AtomIdent::Freeform {
        mean: Cow::Borrowed(ITUNES_MEAN),
        name: Cow::Borrowed(ABRIDGED_TAG_NAME),
    };

    let before = ilst
        .get(&ident_borrowed)
        .and_then(|atom| atom.data().next())
        .and_then(|data| match data {
            AtomData::UTF8(s) | AtomData::UTF16(s) => Some(s.clone()),
            // Other AtomData variants (binary, integer, etc.)
            // aren't conventions for ABRIDGED. Treat them as
            // "unknown prior value" — we'll overwrite.
            _ => None,
        });

    if before.as_deref() == Some(value) {
        return Ok(FieldWriteOutcome::Matched);
    }

    // Insert the new atom. Lofty's `Ilst::insert` replaces any
    // existing atom with the same ident (per the docs), so we
    // don't need a manual remove-then-insert.
    let ident_owned = AtomIdent::Freeform {
        mean: Cow::Owned(ITUNES_MEAN.to_owned()),
        name: Cow::Owned(ABRIDGED_TAG_NAME.to_owned()),
    };
    let atom = Atom::new(ident_owned, AtomData::UTF8(value.to_owned()));
    ilst.insert(atom);
    let _ = mp4.set_ilst(ilst);

    file.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    mp4.save_to(&mut file, WriteOptions::default())
        .map_err(|e| Error::Io(std::io::Error::other(format!("abridged mp4 save: {e}"))))?;

    Ok(FieldWriteOutcome::Changed { before })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::panic,
        reason = "test setup idioms"
    )]

    use super::write_abridged;
    use crate::write::FieldWriteOutcome;

    /// Sanity: a file that isn't recognised as any audio format
    /// returns `Unmapped` (not an error). The `Probe::open` +
    /// `guess_file_type` dispatch path is exercised; the format-
    /// specific writers are not.
    #[test]
    fn non_audio_file_returns_unmapped() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"this is not audio").expect("write");
        let outcome = write_abridged(tmp.path(), "true").expect("probe completes");
        assert_eq!(
            outcome,
            FieldWriteOutcome::Unmapped,
            "non-audio file should yield Unmapped (probe couldn't guess type)"
        );
    }

    /// Generate a minimal valid silent audio file at `path`
    /// using ffmpeg. Returns `None` if ffmpeg isn't on PATH so
    /// the calling test can skip gracefully — CI runners may or
    /// may not have ffmpeg installed.
    ///
    /// `codec` is the `-c:a` argument (e.g. `libmp3lame`,
    /// `aac`); `path` should already have the matching
    /// extension. 0.5 seconds is enough audio to make the file
    /// shape valid for lofty's parser without bloating the test.
    fn ffmpeg_silence(path: &std::path::Path, codec: &str) -> Option<()> {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=mono",
                "-t",
                "0.5",
                "-c:a",
                codec,
                "-b:a",
                "64k",
            ])
            .arg(path)
            .status()
            .ok()?;
        status.success().then_some(())
    }

    /// `ID3v2` path: write Abridged to a silent MP3, then
    /// re-read via lofty and verify the TXXX:ABRIDGED frame
    /// round-trips.
    ///
    /// Skips when ffmpeg isn't on PATH (CI runners without it).
    #[test]
    fn id3v2_round_trip_mp3() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("silence.mp3");
        if ffmpeg_silence(&path, "libmp3lame").is_none() {
            // ffmpeg not on PATH — silent-skip so the test
            // suite still passes on bare runners. Locally
            // (and on macOS CI with homebrew ffmpeg) the test
            // runs.
            return;
        }

        // First write: TXXX absent → Changed { before: None }.
        let outcome = write_abridged(&path, "true").expect("write");
        assert!(
            matches!(outcome, FieldWriteOutcome::Changed { before: None }),
            "first write: expected Changed{{before:None}}, got {outcome:?}"
        );

        // Read back via the typed MpegFile and check the
        // TXXX:ABRIDGED frame actually landed on disk.
        let mpeg_file = <lofty::mpeg::MpegFile as lofty::file::AudioFile>::read_from(
            &mut std::fs::File::open(&path).expect("reopen"),
            lofty::config::ParseOptions::default(),
        )
        .expect("MpegFile parse");
        let id3 = mpeg_file.id3v2().expect("id3v2");
        assert_eq!(
            id3.get_user_text("ABRIDGED"),
            Some("true"),
            "TXXX:ABRIDGED must round-trip; got {:?}",
            id3.get_user_text("ABRIDGED")
        );

        // Idempotence: a second write with the same value
        // returns Matched and doesn't rewrite the file.
        let mtime_before = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        let outcome2 = write_abridged(&path, "true").expect("write");
        assert_eq!(
            outcome2,
            FieldWriteOutcome::Matched,
            "second write with same value: expected Matched"
        );
        let mtime_after = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        assert_eq!(
            mtime_before, mtime_after,
            "Matched outcome must not rewrite the file (mtime invariant)"
        );

        // Update path: write a different value → Changed with
        // `before = Some("true")`.
        let outcome3 = write_abridged(&path, "false").expect("write");
        assert!(
            matches!(&outcome3, FieldWriteOutcome::Changed { before } if before.as_deref() == Some("true")),
            "third write: expected Changed{{before:Some(\"true\")}}, got {outcome3:?}"
        );
    }

    /// MP4 path: write Abridged to a silent M4A, then re-read
    /// via lofty and verify the freeform atom round-trips.
    ///
    /// Skips when ffmpeg isn't on PATH.
    #[test]
    fn mp4_round_trip_m4a() {
        use std::borrow::Cow;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("silence.m4a");
        if ffmpeg_silence(&path, "aac").is_none() {
            // Same silent-skip rationale as id3v2_round_trip_mp3.
            return;
        }

        // First write.
        let outcome = write_abridged(&path, "true").expect("write");
        assert!(
            matches!(outcome, FieldWriteOutcome::Changed { before: None }),
            "first write: expected Changed{{before:None}}, got {outcome:?}"
        );

        // Read back via the typed Mp4File and check the freeform
        // atom landed at the right ident.
        let mp4_file = <lofty::mp4::Mp4File as lofty::file::AudioFile>::read_from(
            &mut std::fs::File::open(&path).expect("reopen"),
            lofty::config::ParseOptions::default(),
        )
        .expect("Mp4File parse");
        let ilst = mp4_file.ilst().expect("ilst");
        let ident = lofty::mp4::AtomIdent::Freeform {
            mean: Cow::Borrowed("com.apple.iTunes"),
            name: Cow::Borrowed("ABRIDGED"),
        };
        let atom = ilst.get(&ident).expect("freeform atom present");
        let data = atom.data().next().expect("at least one data entry");
        match data {
            lofty::mp4::AtomData::UTF8(s) => {
                assert_eq!(s, "true", "freeform atom data must round-trip");
            }
            other => panic!("expected AtomData::UTF8, got {other:?}"),
        }

        // Idempotence.
        let outcome2 = write_abridged(&path, "true").expect("write");
        assert_eq!(outcome2, FieldWriteOutcome::Matched);

        // Update.
        let outcome3 = write_abridged(&path, "false").expect("write");
        assert!(
            matches!(&outcome3, FieldWriteOutcome::Changed { before } if before.as_deref() == Some("true")),
            "third write: expected Changed{{before:Some(\"true\")}}, got {outcome3:?}"
        );
    }
}
