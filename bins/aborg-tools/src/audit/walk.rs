//! Corpus walk + per-book metadata extraction for the
//! audiologo-audit binary (ADR-0054).

#![allow(
    clippy::str_to_string,
    clippy::use_self,
    clippy::doc_overindented_list_items,
    clippy::doc_markdown,
    clippy::too_long_first_doc_paragraph
)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};
use walkdir::WalkDir;

/// Audio extensions the audit walks. AAX is included so the
/// report can flag "skipped (needs aax-decrypt)" rows
/// alongside the playable formats — useful for the operator
/// to see what's pending without disappearing from the report.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "m4b", "m4a", "mp3", "aac", "flac", "ogg", "opus", "wav", "aax",
];

/// Per-source-file metadata captured from a single corpus walk.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Lower-case extension (no leading dot). Useful for the
    /// AAX-skip path.
    pub extension: String,
    /// Book title from `lofty` tags, falling back to the file
    /// stem (no extension) when no title tag is present.
    pub title: String,
    /// Total duration in ms (from lofty's `properties()`).
    /// 0 ms means lofty couldn't read the file's properties —
    /// usually a corrupted / unsupported file; the audit
    /// report flags these without breaking the walk.
    pub duration_ms: u64,
    /// Publisher tag, when present. Useful as a corroborating
    /// signal for audiologo detection — e.g. publisher
    /// "Audible Studios" implies the standard Audible front +
    /// end jingles, while "Brilliance Audio" / "Tantor Audio"
    /// / "Recorded Books" each have their own distinct
    /// signatures. Future detection tier inputs.
    pub publisher: Option<String>,
    /// Copyright tag, when present. Same value as the embedded
    /// `©cpy` MP4 atom or ID3 `TCOP` frame. Holds publisher
    /// imprint info that often disagrees with the publisher
    /// tag (e.g. publisher = "Penguin Audio" + copyright =
    /// "Penguin Random House LLC"). The detection cascade can
    /// fall back to this when publisher is empty.
    pub copyright: Option<String>,
}

impl SourceFile {
    /// True iff the file's extension is in
    /// [`AUDIO_EXTENSIONS`].
    fn is_audio(path: &Path) -> Option<String> {
        let ext = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_lowercase)?;
        if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
            Some(ext)
        } else {
            None
        }
    }

    /// Probe the file's tags + duration via `lofty`. AAX files
    /// won't have a typed lofty implementation; treat them as
    /// "audio with title-from-filename, unknown duration."
    fn probe(path: &Path, extension: &str) -> SourceFile {
        let title_fallback = path
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("(untitled)")
            .to_string();

        // AAX: short-circuit. lofty doesn't decode aavd-tagged
        // files, and we don't want to spam the audit log with
        // its "unknown file type" error for the AAX rows that
        // we know about ahead of time.
        if extension == "aax" {
            return SourceFile {
                path: path.to_path_buf(),
                extension: extension.to_string(),
                title: title_fallback,
                duration_ms: 0,
                publisher: None,
                copyright: None,
            };
        }

        let probe_result = Probe::open(path).and_then(Probe::read);

        let (title, duration_ms, publisher, copyright) = match probe_result {
            Ok(tagged) => {
                let tag = tagged.primary_tag();
                let title = tag
                    .and_then(|t| t.title().map(|s| s.to_string()))
                    .filter(|s| !s.is_empty())
                    .unwrap_or(title_fallback);
                let publisher = tag
                    .and_then(|t| t.get_string(ItemKey::Publisher).map(str::to_string))
                    .filter(|s| !s.is_empty());
                let copyright = tag
                    .and_then(|t| t.get_string(ItemKey::CopyrightMessage).map(str::to_string))
                    .filter(|s| !s.is_empty());
                let duration_ms =
                    u64::try_from(tagged.properties().duration().as_millis()).unwrap_or(u64::MAX);
                (title, duration_ms, publisher, copyright)
            }
            Err(_) => (title_fallback, 0, None, None),
        };

        SourceFile {
            path: path.to_path_buf(),
            extension: extension.to_string(),
            title,
            duration_ms,
            publisher,
            copyright,
        }
    }
}

/// Walk `corpus` recursively, returning every audio file in
/// sorted order. Hidden directories (those starting with `.`)
/// and `__MACOSX` AppleDouble dirs are skipped to keep the
/// report clean.
///
/// # Errors
///
/// Returns the first I/O error encountered during traversal.
/// Per-file probe failures don't abort — they yield a
/// [`SourceFile`] with `duration_ms == 0`, which the report
/// flags as "could not probe" without losing the row.
pub fn walk_corpus(corpus: &Path, limit: Option<usize>) -> Result<Vec<SourceFile>> {
    let mut out = Vec::new();
    let walker = WalkDir::new(corpus)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden dirs and __MACOSX clutter; let
            // regular files through (the audio-ext filter
            // catches them next).
            if !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !(name.starts_with('.') || name == "__MACOSX")
        });

    for entry in walker {
        let entry = entry.context("corpus walk failed")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = SourceFile::is_audio(path) else {
            continue;
        };
        let sf = SourceFile::probe(path, &ext);
        out.push(sf);
        if let Some(n) = limit {
            if out.len() >= n {
                break;
            }
        }
    }

    // Stable ordering — operator's review experience benefits
    // from a deterministic list across runs.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Produce a filesystem-safe slug from `title`. Lowercases
/// ASCII, replaces non-alphanumeric runs with `-`, collapses
/// repeated `-`, trims edge `-`. Caps at 60 chars (room for
/// the `-front-clip.m4a` suffix on a path component cap).
#[must_use]
pub fn slugify(title: &str) -> String {
    let mut s = String::with_capacity(title.len());
    let mut last_was_dash = true; // suppress leading `-`
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            s.push('-');
            last_was_dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        s.push_str("untitled");
    }
    if s.len() > 60 {
        s.truncate(60);
        while s.ends_with('-') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("The Way of Kings"), "the-way-of-kings");
        assert_eq!(slugify("Sanderson, Brandon"), "sanderson-brandon");
        assert_eq!(slugify("!@#$%^&*()"), "untitled");
        assert_eq!(slugify(""), "untitled");
        assert_eq!(slugify("---leading-trailing---"), "leading-trailing");
        assert_eq!(slugify("multiple   spaces"), "multiple-spaces");
    }

    #[test]
    fn slugify_caps_long_input() {
        let s = slugify(&"x".repeat(200));
        assert!(s.len() <= 60);
    }
}
