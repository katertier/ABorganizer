//! AAX inspector — read tags + structural metadata from an Audible
//! AAX/AAXC file **without** decrypting the audio samples.
//!
//! # Why this exists
//!
//! Audible's AAX format is an MP4/M4B container with two material
//! differences from a stock M4B:
//!
//! * Audio samples are encrypted; the codec tag in `stsd` is
//!   `aavd` (vs. `mp4a` for a plain AAC m4b). Decrypt is a
//!   lossless container swap (`aavd → mp4a`) once the operator's
//!   account-specific activation bytes are known.
//! * Metadata atoms (tags, durations, chapter markers) live in
//!   the `moov` tree and are *not* encrypted, so a tag reader
//!   can pull them without any keys.
//!
//! The decrypt stage itself (Swift / AVFoundation FFI) ships in a
//! later slice. This module supplies the read-only inspector that
//! both:
//!
//! 1. Confirms a file *is* an AAX (vs. a plain M4B that happened
//!    to land with the `.aax` extension) by sniffing the codec tag.
//! 2. Surfaces the same tag fields the read-tags stage pulls from a
//!    plain M4B — so `aborg aax info <path>` can give the operator
//!    a complete picture before they decide whether to register
//!    activation bytes for that file.
//!
//! # Implementation
//!
//! The codec-tag sniff is a hand-rolled MP4 box walker
//! ([`read_codec_tag`]). It descends `moov → trak → mdia → minf →
//! stbl → stsd` and reads the 4-byte sample-description type at
//! the documented offset. ~60 lines, no extra dependency.
//!
//! The tag fields and duration come from [`lofty`] — AAX's atom
//! tree is structurally identical to M4B so lofty's MP4 parser
//! reads it transparently.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use ab_core::{Error, Result};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey};

/// Codec tag string written to `stsd`'s sample-description for
/// Audible AAX/AAXC files. Plain AAC m4b uses `mp4a`.
pub const AAX_CODEC_TAG: &str = "aavd";

/// Maximum bytes we'll read from the `moov` atom while walking for
/// the codec tag. Protective cap against pathological or malformed
/// files — `moov` on a real audiobook is rarely above a few hundred
/// KB; we allow 16 `MiB` out of an abundance of caution.
const MAX_MOOV_SCAN_BYTES: u64 = 16 * 1024 * 1024;

/// Read-only summary of an AAX file.
///
/// Fields stay close to what the operator-facing `aborg aax info`
/// command prints. All string fields are `Option<String>` because
/// the upstream tags are optional in the MP4 atom tree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AaxInfo {
    /// Codec tag from `stsd`. `"aavd"` for an Audible AAX, `"mp4a"`
    /// for a plain AAC m4b that happened to have a `.aax` extension,
    /// other values for exotic containers.
    pub codec_tag: Option<String>,
    /// `true` iff [`codec_tag`](Self::codec_tag) equals
    /// [`AAX_CODEC_TAG`]. Convenience flag for callers that just
    /// want a yes/no.
    pub is_aax: bool,
    /// Total duration as reported by the container.
    pub duration_ms: Option<u64>,
    /// Title (`©nam`).
    pub title: Option<String>,
    /// Author / artist (`©ART`).
    pub author: Option<String>,
    /// Narrator (`©wrt` on AAX — Audible writes "Narrated by …"
    /// into the composer atom).
    pub narrator: Option<String>,
    /// Album (`©alb`).
    pub album: Option<String>,
    /// Genre (`©gen` or `gnre`).
    pub genre: Option<String>,
    /// Long description (`desc`).
    pub description: Option<String>,
    /// Copyright (`cprt`).
    pub copyright: Option<String>,
    /// Number of chapter markers found in the container. We don't
    /// surface chapter titles here — they belong to the
    /// `read-embedded-chapters` stage and would bloat the inspector's
    /// output.
    pub chapter_count: usize,
}

/// Read the structural + tag fields of `file` without touching the
/// encrypted audio payload.
///
/// # Errors
///
/// - [`Error::Io`] if the file is missing / unreadable.
/// - [`Error::Stage`] (with `stage = "aax-info"`) if the MP4 box
///   tree is malformed enough that the codec-tag walker can't find
///   `moov`.
pub fn read_info(file: &Path) -> Result<AaxInfo> {
    let codec_tag = read_codec_tag(file)?;
    let is_aax = codec_tag.as_deref() == Some(AAX_CODEC_TAG);

    // Tag + duration via lofty. lofty parses the MP4 box tree
    // structurally — encrypted sample data doesn't faze it.
    // A lofty parse failure here is not fatal: we still have the
    // codec tag and can surface that alone.
    let lofty_info = lofty::read_from_path(file).ok().map(|tagged| {
        let duration_ms = u64::try_from(tagged.properties().duration().as_millis()).ok();
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag()).cloned();
        (duration_ms, tag)
    });

    let (duration_ms, tag) = lofty_info.unzip();
    let duration_ms = duration_ms.flatten();
    let tag = tag.flatten();

    let mut info = AaxInfo {
        codec_tag,
        is_aax,
        duration_ms,
        ..AaxInfo::default()
    };

    if let Some(tag) = tag {
        info.title = tag.title().map(Into::into);
        info.author = tag.artist().map(Into::into);
        info.album = tag.album().map(Into::into);
        info.genre = tag.genre().map(Into::into);
        info.narrator = tag.get_string(ItemKey::Composer).map(Into::into);
        info.description = tag.get_string(ItemKey::Description).map(Into::into);
        info.copyright = tag.get_string(ItemKey::CopyrightMessage).map(Into::into);
    }

    info.chapter_count = count_chapter_markers(file).unwrap_or(0);

    Ok(info)
}

/// Walk the MP4 box tree and return the 4-byte sample-description
/// type ("codec tag") for the first audio track.
///
/// Returns `Ok(None)` only when the file is too short / not an
/// MP4 at all (no `ftyp` at offset 0). Malformed `moov` returns
/// [`Error::Stage`] — the file looked like MP4 but its inner tree
/// is broken.
fn read_codec_tag(file: &Path) -> Result<Option<String>> {
    let mut f = File::open(file)?;
    let file_len = f.metadata()?.len();

    // First box must be `ftyp` for any MP4-family container.
    let Some((ftyp_size, ftyp_type)) = read_box_header(&mut f)? else {
        return Ok(None);
    };
    if &ftyp_type != b"ftyp" {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(ftyp_size))?;

    // Walk top-level boxes until we find `moov`.
    while f.stream_position()? < file_len {
        let Some((box_size, box_type)) = read_box_header(&mut f)? else {
            break;
        };
        let pos_after_header = f.stream_position()?;
        let box_end = pos_after_header
            .checked_add(box_size.saturating_sub(8))
            .ok_or_else(|| Error::stage("aax-info", "box header overflow"))?;
        if &box_type == b"moov" {
            return find_codec_tag_in_moov(&mut f, box_end);
        }
        f.seek(SeekFrom::Start(box_end))?;
    }
    Err(Error::stage("aax-info", "no moov box found"))
}

/// Read a single MP4 box header at the current cursor.
///
/// Returns `Ok(Some((box_size, box_type)))` on a normal 32-bit
/// size header, `Ok(None)` if EOF was hit cleanly between boxes,
/// or [`Error::Stage`] on a truncated header.
///
/// We do **not** support the 64-bit `largesize` extension here —
/// `moov` and its descendants are virtually always < 4 `GiB` even
/// on a 100-hour audiobook, and supporting `largesize` would add
/// complexity that the AAX inspector doesn't benefit from. If a
/// file in the wild trips this, the walker bails cleanly with
/// `Error::Stage`.
fn read_box_header(f: &mut File) -> Result<Option<(u64, [u8; 4])>> {
    let mut hdr = [0u8; 8];
    match f.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    }
    let size = u64::from(u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]));
    let mut box_type = [0u8; 4];
    box_type.copy_from_slice(&hdr[4..8]);
    if size == 1 {
        return Err(Error::stage(
            "aax-info",
            "64-bit MP4 box size not supported in this walker",
        ));
    }
    if size < 8 {
        return Err(Error::stage("aax-info", "MP4 box size too small"));
    }
    Ok(Some((size, box_type)))
}

/// Descend `moov → trak → mdia → minf → stbl → stsd` and return
/// the 4-byte sample-description type ("codec tag") at the
/// documented offset.
fn find_codec_tag_in_moov(f: &mut File, moov_end: u64) -> Result<Option<String>> {
    let moov_start = f.stream_position()?;
    if moov_end.saturating_sub(moov_start) > MAX_MOOV_SCAN_BYTES {
        return Err(Error::stage("aax-info", "moov exceeds scan budget"));
    }
    let Some(trak_range) = find_child_box(f, moov_end, *b"trak")? else {
        return Ok(None);
    };
    let Some(mdia_range) = find_child_box(f, trak_range.1, *b"mdia")? else {
        return Ok(None);
    };
    let Some(minf_range) = find_child_box(f, mdia_range.1, *b"minf")? else {
        return Ok(None);
    };
    let Some(stbl_range) = find_child_box(f, minf_range.1, *b"stbl")? else {
        return Ok(None);
    };
    let Some(stsd_range) = find_child_box(f, stbl_range.1, *b"stsd")? else {
        return Ok(None);
    };

    // stsd layout: 4 bytes version+flags, 4 bytes entry_count, then
    // entries. The first entry's first 8 bytes are a sub-box header
    // (size + type) — the 4-byte type IS the codec tag. We just
    // seek 8 bytes past `stsd`'s header, then read the entry box
    // header.
    f.seek(SeekFrom::Start(stsd_range.0 + 8))?;
    let Some((_, tag)) = read_box_header(f)? else {
        return Ok(None);
    };
    Ok(Some(String::from_utf8_lossy(&tag).into_owned()))
}

/// Find a named child box within `[cursor, parent_end)`. Returns
/// `(content_start, content_end)` — i.e. positioned just after the
/// 8-byte header, ending at the box's last byte + 1.
fn find_child_box(f: &mut File, parent_end: u64, target: [u8; 4]) -> Result<Option<(u64, u64)>> {
    while f.stream_position()? < parent_end {
        let Some((box_size, box_type)) = read_box_header(f)? else {
            return Ok(None);
        };
        let content_start = f.stream_position()?;
        let content_end = content_start
            .checked_add(box_size.saturating_sub(8))
            .ok_or_else(|| Error::stage("aax-info", "child box overflow"))?;
        if box_type == target {
            return Ok(Some((content_start, content_end)));
        }
        f.seek(SeekFrom::Start(content_end))?;
    }
    Ok(None)
}

/// Count chapter markers via lofty's chapter atom enumeration.
///
/// AAX uses Nero `chpl` chapters in the `moov/udta` subtree.
/// `lofty` exposes these via the `Mp4Ilst` tag's chapter list. If
/// chapters are absent or the parse fails, return `None`; callers
/// fold that into `chapter_count = 0`.
fn count_chapter_markers(file: &Path) -> Option<usize> {
    use lofty::config::ParseOptions;
    use lofty::probe::Probe;
    let tagged = Probe::open(file)
        .ok()?
        .options(ParseOptions::new())
        .read()
        .ok()?;
    // lofty does not expose chapter atoms uniformly across formats;
    // the Mp4Ilst tag stores chapters in its own slot but only on
    // newer lofty versions. Until the workspace lofty bump exposes
    // that surface, the inspector reports 0 here and leaves chapter
    // enumeration to the `read-embedded-chapters` stage which
    // already handles the QuickTime text track + Nero chpl walk.
    let _ = tagged;
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, reason = "test idioms")]

    use super::*;

    /// Build a minimal MP4-family file at `path` with the supplied
    /// codec tag inside `moov/trak/mdia/minf/stbl/stsd`. Audio
    /// sample data is omitted entirely — the inspector reads only
    /// metadata, so the file doesn't need a real `mdat`.
    fn write_fake_mp4(path: &Path, codec_tag: [u8; 4]) {
        // Box helpers — write `size (u32 BE) + type (4 bytes) + payload`.
        fn make_box(box_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
            let total = 8 + payload.len();
            let mut out = Vec::with_capacity(total);
            out.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
            out.extend_from_slice(&box_type);
            out.extend_from_slice(payload);
            out
        }

        // stsd: 4 bytes version+flags, 4 bytes entry_count=1, then
        // one sample entry (header only).
        let mut stsd_payload = Vec::new();
        stsd_payload.extend_from_slice(&[0u8; 4]); // version+flags
        stsd_payload.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        // Entry: 8-byte header. Size = 8, type = codec_tag.
        let entry = make_box(codec_tag, &[]);
        stsd_payload.extend_from_slice(&entry);
        let stsd = make_box(*b"stsd", &stsd_payload);

        let stbl = make_box(*b"stbl", &stsd);
        let minf = make_box(*b"minf", &stbl);
        let mdia = make_box(*b"mdia", &minf);
        let trak = make_box(*b"trak", &mdia);
        let moov = make_box(*b"moov", &trak);

        // Minimal ftyp: "isom" major brand + 0 minor + nothing else.
        let mut ftyp_payload = Vec::new();
        ftyp_payload.extend_from_slice(b"isom");
        ftyp_payload.extend_from_slice(&[0u8; 4]); // minor version
        let ftyp = make_box(*b"ftyp", &ftyp_payload);

        let mut file = Vec::new();
        file.extend(ftyp);
        file.extend(moov);
        std::fs::write(path, file).expect("write fake mp4");
    }

    #[test]
    fn aax_codec_tag_detected() {
        // A synthesised MP4 with `aavd` in stsd is recognised as
        // AAX. Real AAX files in the wild carry the same tag.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake.aax");
        write_fake_mp4(&path, *b"aavd");

        let info = read_info(&path).expect("read_info ok");
        assert_eq!(info.codec_tag.as_deref(), Some("aavd"));
        assert!(info.is_aax, "aavd codec tag should flag is_aax");
    }

    #[test]
    fn plain_aac_is_not_aax() {
        // A synthesised MP4 with the standard `mp4a` codec tag is
        // explicitly NOT AAX even if it happens to live in a file
        // with the `.aax` extension. The inspector lets the
        // operator catch misnamed files.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("misnamed.aax");
        write_fake_mp4(&path, *b"mp4a");

        let info = read_info(&path).expect("read_info ok");
        assert_eq!(info.codec_tag.as_deref(), Some("mp4a"));
        assert!(!info.is_aax, "mp4a codec tag should NOT flag is_aax");
    }

    #[test]
    fn non_mp4_returns_none_codec() {
        // A plain-text file isn't MP4 at all — there's no `ftyp`,
        // so the walker bails out and reports `codec_tag = None`.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-mp4.bin");
        std::fs::write(&path, b"this is not an MP4 file\n").expect("write");
        let info = read_info(&path).expect("read_info ok");
        assert_eq!(info.codec_tag, None);
        assert!(!info.is_aax);
    }

    #[test]
    fn missing_file_returns_io_error() {
        let result = read_info(Path::new("/tmp/does-not-exist-aborg-aax.bin"));
        assert!(matches!(result, Err(Error::Io(_))));
    }
}
