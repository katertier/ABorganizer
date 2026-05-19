//! Format-specific writer for the `COLLECTION_NAME` /
//! `COLLECTION_TYPE` tag pair.
//!
//! Collections (box-sets, compilations, curated lists — see
//! `book_collections.kind`) are first-class metadata in
//! ABorganizer's DB but have no standard tag-vocabulary slot. The
//! ID3 / Vorbis / MP4 cores don't define "collection" as a
//! built-in field; the convention here mirrors what
//! [`crate::abridged`] does for the abridged flag:
//!
//! - **`ID3v2`** (MP3 + AAC + WAV + AIFF): two `TXXX` "user-
//!   defined text" frames with descriptions `COLLECTION_NAME` and
//!   `COLLECTION_TYPE`. Readable in mp3tag, `MusicBrainz` Picard,
//!   Foobar2000, and every other ID3-aware tool out of the box.
//! - **MP4** (m4a + m4b): two freeform atoms
//!   `----:com.apple.iTunes:COLLECTION_NAME` and
//!   `----:com.apple.iTunes:COLLECTION_TYPE`. Visible to `MP4Box`,
//!   `AtomicParsley`, and any iTunes-style tag editor.
//!
//! Other formats (`FLAC` / Vorbis / Opus / APE / Speex /
//! `WavPack`) return [`CollectionPairOutcome::Unmapped`]. They
//! support custom comments natively, but the audiobook user base
//! on those formats is small enough that the dispatch isn't worth
//! it this slice.
//!
//! ## Multi-collection
//!
//! Slice 1 supports a single collection per book. When a book is
//! in multiple collections the caller picks one (typically the
//! earliest membership by `added_at`) and a tracing warning is
//! emitted. Multi-collection encoding (repeated frames on ID3v2.4
//! / multiple atoms on MP4) lands in a follow-up slice once the
//! shape is exercised in real catalogues.

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

/// TXXX description for the collection's display name. Single
/// shared constant so the ID3 + MP4 branches can't drift on
/// capitalization (the on-disk frame would otherwise be a
/// different tag on a typo).
pub(crate) const COLLECTION_NAME_TAG: &str = "COLLECTION_NAME";
/// TXXX description for the collection's `kind` field
/// (`box_set` / `compilation` / `curated`). Same drift-prevention
/// rationale as [`COLLECTION_NAME_TAG`].
pub(crate) const COLLECTION_TYPE_TAG: &str = "COLLECTION_TYPE";

/// Outcome of a single-file collection-tag pair write.
///
/// The pair shape (name + type) means the outcomes apply to both
/// frames atomically: if either differs from the on-disk value,
/// the file is rewritten with both frames updated. The variant
/// distinguishes the three states the caller's report needs to
/// track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionPairOutcome {
    /// At least one frame moved; file was rewritten. `before_name`
    /// and `before_type` carry the prior on-disk values for the
    /// stage's audit-log writer (`None` = tag was absent).
    Changed {
        before_name: Option<String>,
        before_type: Option<String>,
    },
    /// Both frames already match what was requested. File not
    /// rewritten; mtime preserved.
    Matched,
    /// File format has no collection-tag mapping (FLAC / Vorbis /
    /// Opus / APE / Speex / `WavPack` / probe failure). No write
    /// attempted.
    Unmapped,
}

/// Write the `COLLECTION_NAME` / `COLLECTION_TYPE` tag pair to
/// `path` as a format-specific custom tag.
///
/// # Errors
///
/// - [`Error::Io`] if the file can't be opened / probed / parsed
///   by lofty or if the typed save fails.
pub fn write_collection_pair(path: &Path, name: &str, kind: &str) -> Result<CollectionPairOutcome> {
    let probe = Probe::open(path)
        .map_err(|e| Error::Io(std::io::Error::other(format!("collection probe open: {e}"))))?;
    let probe = probe.guess_file_type().map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "collection guess_file_type: {e}"
        )))
    })?;

    match probe.file_type() {
        Some(FileType::Mpeg | FileType::Aac | FileType::Wav | FileType::Aiff) => {
            write_collection_id3v2(path, name, kind)
        }
        Some(FileType::Mp4) => write_collection_mp4(path, name, kind),
        _ => Ok(CollectionPairOutcome::Unmapped),
    }
}

/// `ID3v2` writer: `TXXX:COLLECTION_NAME` + `TXXX:COLLECTION_TYPE`.
fn write_collection_id3v2(path: &Path, name: &str, kind: &str) -> Result<CollectionPairOutcome> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(Error::Io)?;
    let mut mpeg = <MpegFile as AudioFile>::read_from(&mut file, ParseOptions::default())
        .map_err(|e| Error::Io(std::io::Error::other(format!("collection id3 read: {e}"))))?;

    let mut id3v2 = mpeg.id3v2_mut().map(std::mem::take).unwrap_or_default();

    let before_name = id3v2.get_user_text(COLLECTION_NAME_TAG).map(str::to_owned);
    let before_type = id3v2.get_user_text(COLLECTION_TYPE_TAG).map(str::to_owned);
    if before_name.as_deref() == Some(name) && before_type.as_deref() == Some(kind) {
        return Ok(CollectionPairOutcome::Matched);
    }

    let _ = id3v2.insert_user_text(COLLECTION_NAME_TAG.to_owned(), name.to_owned());
    let _ = id3v2.insert_user_text(COLLECTION_TYPE_TAG.to_owned(), kind.to_owned());
    let _ = mpeg.set_id3v2(id3v2);

    file.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    mpeg.save_to(&mut file, WriteOptions::default())
        .map_err(|e| Error::Io(std::io::Error::other(format!("collection id3 save: {e}"))))?;

    Ok(CollectionPairOutcome::Changed {
        before_name,
        before_type,
    })
}

/// MP4 writer: freeform iTunes atoms for both frames.
fn write_collection_mp4(path: &Path, name: &str, kind: &str) -> Result<CollectionPairOutcome> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(Error::Io)?;
    let mut mp4 = <Mp4File as AudioFile>::read_from(&mut file, ParseOptions::default())
        .map_err(|e| Error::Io(std::io::Error::other(format!("collection mp4 read: {e}"))))?;

    let mut ilst: Ilst = mp4.ilst_mut().map(std::mem::take).unwrap_or_default();

    let before_name = read_freeform_utf8(&ilst, COLLECTION_NAME_TAG);
    let before_type = read_freeform_utf8(&ilst, COLLECTION_TYPE_TAG);
    if before_name.as_deref() == Some(name) && before_type.as_deref() == Some(kind) {
        return Ok(CollectionPairOutcome::Matched);
    }

    insert_freeform_utf8(&mut ilst, COLLECTION_NAME_TAG, name);
    insert_freeform_utf8(&mut ilst, COLLECTION_TYPE_TAG, kind);
    let _ = mp4.set_ilst(ilst);

    file.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    mp4.save_to(&mut file, WriteOptions::default())
        .map_err(|e| Error::Io(std::io::Error::other(format!("collection mp4 save: {e}"))))?;

    Ok(CollectionPairOutcome::Changed {
        before_name,
        before_type,
    })
}

fn read_freeform_utf8(ilst: &Ilst, name: &str) -> Option<String> {
    let ident = AtomIdent::Freeform {
        mean: Cow::Borrowed(ITUNES_MEAN),
        name: Cow::Borrowed(name),
    };
    ilst.get(&ident)
        .and_then(|atom| atom.data().next())
        .and_then(|data| match data {
            AtomData::UTF8(s) | AtomData::UTF16(s) => Some(s.clone()),
            _ => None,
        })
}

fn insert_freeform_utf8(ilst: &mut Ilst, name: &str, value: &str) {
    let ident = AtomIdent::Freeform {
        mean: Cow::Owned(ITUNES_MEAN.to_owned()),
        name: Cow::Owned(name.to_owned()),
    };
    let atom = Atom::new(ident, AtomData::UTF8(value.to_owned()));
    ilst.insert(atom);
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::panic,
        reason = "test setup idioms"
    )]

    use super::{
        COLLECTION_NAME_TAG, COLLECTION_TYPE_TAG, CollectionPairOutcome, write_collection_pair,
    };

    #[test]
    fn non_audio_file_returns_unmapped() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"not audio").expect("write");
        let outcome = write_collection_pair(tmp.path(), "Foundation Trilogy", "box_set")
            .expect("probe completes");
        assert_eq!(outcome, CollectionPairOutcome::Unmapped);
    }

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

    #[test]
    fn id3v2_round_trip_mp3() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("silence.mp3");
        if ffmpeg_silence(&path, "libmp3lame").is_none() {
            return;
        }

        // First write: both TXXX absent → Changed { before_*: None }.
        let outcome = write_collection_pair(&path, "Foundation Trilogy", "box_set").expect("write");
        assert!(
            matches!(
                outcome,
                CollectionPairOutcome::Changed {
                    before_name: None,
                    before_type: None,
                }
            ),
            "first write expected Changed{{None,None}}, got {outcome:?}"
        );

        // Verify on-disk landing via typed read.
        let mpeg = <lofty::mpeg::MpegFile as lofty::file::AudioFile>::read_from(
            &mut std::fs::File::open(&path).expect("reopen"),
            lofty::config::ParseOptions::default(),
        )
        .expect("parse");
        let id3 = mpeg.id3v2().expect("id3v2");
        assert_eq!(
            id3.get_user_text(COLLECTION_NAME_TAG),
            Some("Foundation Trilogy")
        );
        assert_eq!(id3.get_user_text(COLLECTION_TYPE_TAG), Some("box_set"));

        // Idempotence.
        let mtime_before = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        let outcome2 =
            write_collection_pair(&path, "Foundation Trilogy", "box_set").expect("write");
        assert_eq!(outcome2, CollectionPairOutcome::Matched);
        let mtime_after = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        assert_eq!(mtime_before, mtime_after, "Matched must not rewrite");

        // Update: changing one frame triggers Changed with both befores set.
        let outcome3 =
            write_collection_pair(&path, "Foundation Trilogy", "compilation").expect("write");
        match outcome3 {
            CollectionPairOutcome::Changed {
                before_name,
                before_type,
            } => {
                assert_eq!(before_name.as_deref(), Some("Foundation Trilogy"));
                assert_eq!(before_type.as_deref(), Some("box_set"));
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn mp4_round_trip_m4a() {
        use std::borrow::Cow;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("silence.m4a");
        if ffmpeg_silence(&path, "aac").is_none() {
            return;
        }

        let outcome = write_collection_pair(&path, "Asimov Anthology", "curated").expect("write");
        assert!(matches!(
            outcome,
            CollectionPairOutcome::Changed {
                before_name: None,
                before_type: None,
            }
        ));

        let mp4 = <lofty::mp4::Mp4File as lofty::file::AudioFile>::read_from(
            &mut std::fs::File::open(&path).expect("reopen"),
            lofty::config::ParseOptions::default(),
        )
        .expect("parse");
        let ilst = mp4.ilst().expect("ilst");
        for (key, expected) in [
            (COLLECTION_NAME_TAG, "Asimov Anthology"),
            (COLLECTION_TYPE_TAG, "curated"),
        ] {
            let ident = lofty::mp4::AtomIdent::Freeform {
                mean: Cow::Borrowed("com.apple.iTunes"),
                name: Cow::Borrowed(key),
            };
            let atom = ilst.get(&ident).expect("atom present");
            match atom.data().next().expect("data") {
                lofty::mp4::AtomData::UTF8(s) => assert_eq!(s, expected, "{key} mismatch"),
                other => panic!("expected UTF8 for {key}, got {other:?}"),
            }
        }

        let outcome2 = write_collection_pair(&path, "Asimov Anthology", "curated").expect("write");
        assert_eq!(outcome2, CollectionPairOutcome::Matched);
    }
}
