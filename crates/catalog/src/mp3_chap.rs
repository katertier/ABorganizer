//! `ID3v2` `CHAP` frame decoder — MP3 embedded chapter import.
//!
//! `lofty` 0.24 doesn't know about the `ID3v2` `CHAP` frame
//! (chapter info) or `CTOC` (chapter table-of-contents); it
//! surfaces unknown frames as [`lofty::id3::v2::BinaryFrame`]
//! with the raw body bytes intact. This module hand-rolls the
//! decoder on those bytes so the existing `embedded-chapters`
//! stage (slice 2H) can pick up MP3 chapter data in addition
//! to the MP4 `chpl` / chapter-track support it already has.
//!
//! ## Frame format (`ID3v2.4` spec § 4.30)
//!
//! ```text
//! <CHAP frame body>
//!   Element ID    <text string> $00
//!   Start time    $xx xx xx xx          (4 bytes BE u32, ms)
//!   End time      $xx xx xx xx
//!   Start offset  $xx xx xx xx          ($FF×4 = unset)
//!   End offset    $xx xx xx xx
//!   <Embedded sub-frames>                (typically TIT2 = title)
//! ```
//!
//! Sub-frames carry their own 10-byte `ID3v2` frame header
//! (4-byte ID, 4-byte size, 2-byte flags) followed by the body.
//! In `ID3v2.4` the size is "synchsafe" (28 bits packed into 4
//! bytes with each high bit clear); `ID3v2.3` uses a plain BE
//! u32. The version travels via
//! [`lofty::id3::v2::Id3v2Tag::original_version`] so we dispatch
//! the right reader.
//!
//! ## Scope
//!
//! - Returns `(start_ms, title)` tuples in file-local time,
//!   matching the existing MP4 reader's shape.
//! - Reads chapter titles from the `TIT2` sub-frame (text
//!   information). Other sub-frames (`TIT3` subtitle, `WXXX`
//!   URL, `APIC` cover) are ignored — title is the only field
//!   the chapter table records today.
//! - Malformed CHAP frames log + skip; one bad chapter doesn't
//!   take the whole file's chapter list down.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::id3::v2::{Frame, Id3v2Version};
use lofty::mpeg::MpegFile;

/// Read CHAP-frame chapters from an MP3 file.
///
/// Returns `(start_ms, title)` tuples sorted by `start_ms`.
/// Files with no `ID3v2` tag, no CHAP frames, or unreadable
/// frames return an empty vector — no error type; the
/// embedded-chapters stage treats absent chapters as "this file
/// simply doesn't contribute" rather than a fatal condition.
///
/// Uses [`MpegFile::read_from`] directly (rather than the generic
/// [`lofty::read_from_path`]) because the public `TaggedFile` API
/// only exposes the lossy [`lofty::tag::Tag`] view of frames,
/// which strips the binary body we need for `CHAP`. `MpegFile`
/// keeps the typed [`lofty::id3::v2::Id3v2Tag`] available.
#[must_use]
pub fn read_chapters_from_mp3(path: &Path) -> Vec<(u64, String)> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(
                file = %path.display(),
                error = %e,
                "mp3_chap.open_failed"
            );
            return Vec::new();
        }
    };
    let mut reader = BufReader::new(file);
    let mpeg = match MpegFile::read_from(&mut reader, ParseOptions::new()) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(
                file = %path.display(),
                error = %e,
                "mp3_chap.read_failed"
            );
            return Vec::new();
        }
    };
    let Some(id3v2) = mpeg.id3v2() else {
        return Vec::new();
    };
    let version = id3v2.original_version();
    let mut chapters: Vec<(u64, String)> = Vec::new();
    for frame in id3v2 {
        let Frame::Binary(bf) = frame else { continue };
        if bf.id().as_str() != "CHAP" {
            continue;
        }
        match parse_chap_body(&bf.data, version) {
            Ok(Some(ch)) => chapters.push(ch),
            Ok(None) => {
                // Valid frame, no title sub-frame — still useful
                // as a timeline anchor. Synthesize a placeholder
                // so the player gets continuous coverage.
                chapters.push((0, "Chapter".to_owned()));
            }
            Err(e) => {
                tracing::warn!(
                    file = %path.display(),
                    error = %e,
                    "mp3_chap.parse_failed"
                );
            }
        }
    }
    chapters.sort_by_key(|(start, _)| *start);
    chapters
}

/// Error returned by [`parse_chap_body`] when a CHAP frame's
/// bytes don't conform to the spec. Carried as a static string;
/// the caller logs + skips the frame, no error propagation.
#[derive(Debug)]
pub struct ChapParseError(&'static str);

impl std::fmt::Display for ChapParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CHAP frame parse error: {}", self.0)
    }
}

impl std::error::Error for ChapParseError {}

/// Decode one CHAP frame's body bytes. Returns:
///
/// - `Ok(Some((start_ms, title)))` when the body parses cleanly
///   and a `TIT2` sub-frame yields a non-empty title.
/// - `Ok(None)` when the body parses cleanly but no `TIT2`
///   sub-frame was found — the caller decides whether to
///   synthesize a placeholder.
/// - `Err(ChapParseError)` when the bytes are short / malformed.
///
/// # Errors
///
/// Returns [`ChapParseError`] on a truncated header or an
/// invalid embedded sub-frame.
pub fn parse_chap_body(
    body: &[u8],
    version: Id3v2Version,
) -> Result<Option<(u64, String)>, ChapParseError> {
    // Skip the null-terminated element ID.
    let elem_end = body
        .iter()
        .position(|b| *b == 0)
        .ok_or(ChapParseError("missing element-id terminator"))?;
    // 4 × u32 = 16 bytes of timing/offset fields follow.
    let after_id = &body[elem_end + 1..];
    if after_id.len() < 16 {
        return Err(ChapParseError("body shorter than 16-byte timing block"));
    }
    let start_ms_u32 = u32::from_be_bytes([after_id[0], after_id[1], after_id[2], after_id[3]]);
    let _end_ms = u32::from_be_bytes([after_id[4], after_id[5], after_id[6], after_id[7]]);
    let _start_offset = u32::from_be_bytes([after_id[8], after_id[9], after_id[10], after_id[11]]);
    let _end_offset = u32::from_be_bytes([after_id[12], after_id[13], after_id[14], after_id[15]]);
    let sub_frames = &after_id[16..];
    let title = walk_sub_frames_for_title(sub_frames, version);
    Ok(title.map(|t| (u64::from(start_ms_u32), t)))
}

/// Walk the CHAP frame's embedded sub-frames, return the first
/// `TIT2` (Title/Songname/Content description) value.
///
/// Sub-frame header layout matches the outer `ID3v2` frame format:
///
/// ```text
///   Frame ID   (4 bytes ASCII)
///   Size       (4 bytes; synchsafe in v2.4, plain BE u32 in v2.3)
///   Flags      (2 bytes; ignored — we just need the bytes)
///   Body       (Size bytes)
/// ```
fn walk_sub_frames_for_title(sub_frames: &[u8], version: Id3v2Version) -> Option<String> {
    let mut cursor = 0usize;
    while cursor + 10 <= sub_frames.len() {
        let id = &sub_frames[cursor..cursor + 4];
        let size = decode_frame_size(&sub_frames[cursor + 4..cursor + 8], version);
        // Skip the 2-byte flags.
        let body_start = cursor + 10;
        let body_end = body_start.checked_add(size)?;
        if body_end > sub_frames.len() {
            return None;
        }
        if id == b"TIT2" {
            let body = &sub_frames[body_start..body_end];
            return decode_text_frame(body);
        }
        cursor = body_end;
    }
    None
}

/// Read a 4-byte frame size, dispatching on the tag version's
/// encoding. v2.4 = synchsafe (28-bit, each byte's high bit clear);
/// v2.3 / v2.2 = plain big-endian u32.
fn decode_frame_size(bytes: &[u8], version: Id3v2Version) -> usize {
    debug_assert_eq!(bytes.len(), 4);
    let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
    match version {
        Id3v2Version::V4 => {
            // Synchsafe: 4 bytes × 7 bits = 28-bit size.
            let n = (u32::from(raw[0]) << 21)
                | (u32::from(raw[1]) << 14)
                | (u32::from(raw[2]) << 7)
                | u32::from(raw[3]);
            n as usize
        }
        _ => u32::from_be_bytes(raw) as usize,
    }
}

/// Decode a TIT2 (text information) frame body. The first byte
/// is the text encoding ($00 ISO-8859-1, $01 UTF-16 with BOM,
/// $02 UTF-16BE, $03 UTF-8). Trailing terminator nulls are
/// stripped *after* decoding so UTF-16 code units of the form
/// `0xXX 0x00` (any ASCII char) aren't mistaken for padding.
fn decode_text_frame(body: &[u8]) -> Option<String> {
    let (encoding, rest) = body.split_first()?;
    if rest.is_empty() {
        return None;
    }
    let raw = match *encoding {
        0x00 => {
            // ISO-8859-1: every byte is one code point. Map
            // bytes directly to chars.
            rest.iter().map(|b| *b as char).collect::<String>()
        }
        0x03 => {
            // UTF-8: try a direct decode.
            std::str::from_utf8(rest).ok()?.to_owned()
        }
        0x01 | 0x02 => decode_utf16_text(*encoding, rest)?,
        _ => return None,
    };
    let trimmed = raw.trim_end_matches('\0');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Decode a UTF-16 text frame body (encoding $01 with BOM, or
/// $02 big-endian without BOM). Returns the decoded `String`
/// including any trailing U+0000 terminator; the caller strips
/// that post-decode.
fn decode_utf16_text(encoding: u8, payload: &[u8]) -> Option<String> {
    let (big_endian, offset) = if encoding == 0x02 {
        (true, 0)
    } else {
        // 0x01 — BOM-prefixed. 0xFEFF (BE) or 0xFFFE (LE);
        // missing BOM defaults to little-endian (lofty's
        // behaviour for malformed v2.3 tags).
        if payload.len() < 2 {
            return None;
        }
        match (payload[0], payload[1]) {
            (0xFE, 0xFF) => (true, 2),
            (0xFF, 0xFE) => (false, 2),
            _ => (false, 0),
        }
    };
    let body = &payload[offset..];
    if body.len() < 2 {
        return None;
    }
    let mut units: Vec<u16> = Vec::with_capacity(body.len() / 2);
    for chunk in body.chunks_exact(2) {
        let u = if big_endian {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_le_bytes([chunk[0], chunk[1]])
        };
        units.push(u);
    }
    Some(String::from_utf16_lossy(&units))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// Build a synthetic CHAP frame body matching the spec.
    /// `title` lands in a `TIT2` sub-frame; `version` selects
    /// the sub-frame size encoding.
    fn build_chap_body(
        elem_id: &[u8],
        start_ms: u32,
        end_ms: u32,
        title_utf8: &str,
        version: Id3v2Version,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(elem_id);
        out.push(0); // null terminator
        out.extend_from_slice(&start_ms.to_be_bytes());
        out.extend_from_slice(&end_ms.to_be_bytes());
        out.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // start_offset unset
        out.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // end_offset unset
        // TIT2 sub-frame: 4-byte ID, 4-byte size, 2-byte flags,
        // then body = encoding($03=UTF-8) + UTF-8 bytes.
        let mut body = vec![0x03_u8]; // UTF-8
        body.extend_from_slice(title_utf8.as_bytes());
        out.extend_from_slice(b"TIT2");
        let size_bytes = encode_frame_size(body.len(), version);
        out.extend_from_slice(&size_bytes);
        out.extend_from_slice(&[0x00, 0x00]); // flags
        out.extend_from_slice(&body);
        out
    }

    /// Reverse of `decode_frame_size` for the test fixture
    /// builder.
    fn encode_frame_size(size: usize, version: Id3v2Version) -> [u8; 4] {
        let n = u32::try_from(size).expect("test fixture size fits u32");
        match version {
            Id3v2Version::V4 => {
                // Synchsafe: split 28 bits into 4 7-bit groups,
                // each as a byte with high bit clear.
                [
                    ((n >> 21) & 0x7F) as u8,
                    ((n >> 14) & 0x7F) as u8,
                    ((n >> 7) & 0x7F) as u8,
                    (n & 0x7F) as u8,
                ]
            }
            _ => n.to_be_bytes(),
        }
    }

    #[test]
    fn parses_well_formed_v4_chap() {
        let body = build_chap_body(b"ch1", 1_000, 5_000, "Chapter 1", Id3v2Version::V4);
        let got = parse_chap_body(&body, Id3v2Version::V4).expect("parse");
        assert_eq!(got, Some((1_000, "Chapter 1".to_owned())));
    }

    #[test]
    fn parses_well_formed_v3_chap() {
        let body = build_chap_body(b"ch2", 60_000, 120_000, "Part Two", Id3v2Version::V3);
        let got = parse_chap_body(&body, Id3v2Version::V3).expect("parse");
        assert_eq!(got, Some((60_000, "Part Two".to_owned())));
    }

    #[test]
    fn parses_iso_8859_1_title() {
        // ISO-8859-1 with high-byte chars (encoding $00).
        let mut body = Vec::new();
        body.extend_from_slice(b"ch3\x00");
        body.extend_from_slice(&500_u32.to_be_bytes());
        body.extend_from_slice(&1500_u32.to_be_bytes());
        body.extend_from_slice(&[0xFF; 4]);
        body.extend_from_slice(&[0xFF; 4]);
        body.extend_from_slice(b"TIT2");
        let title_body = b"\x00Caf\xe9"; // encoding=$00, "Café"
        body.extend_from_slice(&encode_frame_size(title_body.len(), Id3v2Version::V4));
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(title_body);
        let got = parse_chap_body(&body, Id3v2Version::V4).expect("parse");
        assert_eq!(got, Some((500, "Café".to_owned())));
    }

    #[test]
    fn parses_utf16_with_bom_title() {
        // UTF-16 with BOM (encoding $01). "Hi" little-endian.
        let mut body = Vec::new();
        body.extend_from_slice(b"ch4\x00");
        body.extend_from_slice(&0_u32.to_be_bytes());
        body.extend_from_slice(&0_u32.to_be_bytes());
        body.extend_from_slice(&[0xFF; 4]);
        body.extend_from_slice(&[0xFF; 4]);
        body.extend_from_slice(b"TIT2");
        // encoding=$01, BOM = FF FE (LE), then "Hi" as UTF-16LE.
        let title_body = b"\x01\xff\xfeH\x00i\x00";
        body.extend_from_slice(&encode_frame_size(title_body.len(), Id3v2Version::V4));
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(title_body);
        let got = parse_chap_body(&body, Id3v2Version::V4).expect("parse");
        assert_eq!(got, Some((0, "Hi".to_owned())));
    }

    #[test]
    fn missing_title_subframe_returns_none() {
        // Spec-compliant CHAP body but no TIT2 sub-frame at all.
        let mut body = Vec::new();
        body.extend_from_slice(b"ch5\x00");
        body.extend_from_slice(&123_u32.to_be_bytes());
        body.extend_from_slice(&456_u32.to_be_bytes());
        body.extend_from_slice(&[0; 4]);
        body.extend_from_slice(&[0; 4]);
        let got = parse_chap_body(&body, Id3v2Version::V4).expect("parse");
        assert_eq!(got, None);
    }

    #[test]
    fn truncated_body_errors() {
        // Only the element ID — nothing else.
        let body = b"ch6\x00".to_vec();
        let got = parse_chap_body(&body, Id3v2Version::V4);
        assert!(got.is_err(), "expected error on 4-byte body");
    }

    #[test]
    fn missing_elem_id_terminator_errors() {
        // No null byte anywhere → can't find element ID end.
        let body = b"chunk".to_vec();
        let got = parse_chap_body(&body, Id3v2Version::V4);
        assert!(got.is_err());
    }

    #[test]
    fn decode_text_frame_strips_post_decode_null_terminator() {
        // UTF-8 with a trailing null terminator (some encoders add
        // one to text frames).
        let body = b"\x03Hello\x00";
        let got = decode_text_frame(body);
        assert_eq!(got, Some("Hello".to_owned()));
    }

    #[test]
    fn decode_text_frame_preserves_ascii_in_utf16le() {
        // Regression: byte-level null trim used to corrupt UTF-16
        // bodies because the trailing `0x00` byte of `i\0` looked
        // like padding. With encoding-aware trimming the trailing
        // U+0000 code unit is stripped post-decode, not pre.
        let body = b"\x01\xff\xfeH\x00i\x00\x00\x00";
        let got = decode_text_frame(body);
        assert_eq!(got, Some("Hi".to_owned()));
    }

    #[test]
    fn decode_text_frame_handles_utf16be() {
        // Encoding $02 — UTF-16BE without BOM.
        let body = b"\x02\x00H\x00i";
        let got = decode_text_frame(body);
        assert_eq!(got, Some("Hi".to_owned()));
    }

    #[test]
    fn synchsafe_round_trips() {
        for n in [0_usize, 1, 127, 128, 16_383, 16_384, 100_000, 0x0F_FF_FF_FF] {
            let bytes = encode_frame_size(n, Id3v2Version::V4);
            // Every byte must have high bit clear.
            for b in bytes {
                assert_eq!(b & 0x80, 0, "synchsafe byte 0x{b:02x} has high bit set");
            }
            let back = decode_frame_size(&bytes, Id3v2Version::V4);
            assert_eq!(back, n);
        }
    }

    #[test]
    fn plain_be_round_trips_for_v3() {
        for n in [0_usize, 1, 0xFFFF, 0x1234_5678] {
            let bytes = encode_frame_size(n, Id3v2Version::V3);
            let back = decode_frame_size(&bytes, Id3v2Version::V3);
            assert_eq!(back, n);
        }
    }
}
