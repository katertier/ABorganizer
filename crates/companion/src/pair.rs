//! Auto-pair geometry rule (ADR-0043).
//!
//! Given a companion file's path and a list of audiobook subtree
//! roots (each is the audiobook's containing directory), decide:
//!
//! - **Paired** — exactly one audiobook's subtree claims the
//!   companion. Set `book_companions.book_id` to that audiobook.
//! - **Ambiguous** — multiple audiobooks share an ancestor that
//!   contains the companion. Leave `book_id = NULL` and record a
//!   `companion_nearby_books` junction row per candidate so the
//!   ❓ heuristic has its data.
//! - **Unpaired** — no audiobook's subtree contains the
//!   companion. True orphan. Leave `book_id = NULL`; no junction
//!   rows.
//!
//! "Subtree contains the companion" = the companion path is the
//! audiobook directory itself OR any descendant. The ADR's
//! 2026-05-14 refinement: subdirectories of the audiobook's
//! directory also auto-pair, not just immediate siblings. The
//! pure-function shape lives here; DB wiring is a follow-up
//! slice.

use std::path::{Path, PathBuf};

use crate::BookKey;

/// Outcome of [`auto_pair`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoPairResult<K: BookKey> {
    /// Exactly one audiobook subtree claimed the companion.
    Paired(K),
    /// The companion sits under multiple audiobooks' shared
    /// ancestor. The vec holds every candidate in `audiobooks`
    /// whose directory ancestor-relates to the companion path;
    /// caller inserts a `companion_nearby_books` junction row
    /// per entry.
    Ambiguous(Vec<K>),
    /// No audiobook directory contains the companion. True
    /// orphan — `book_id` stays NULL with no junction rows.
    Unpaired,
}

/// A candidate audiobook for pairing — `(key, directory)`. The
/// `key` is whatever the caller uses to identify the book; the
/// directory is the audiobook's containing folder on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudiobookCandidate<K: BookKey> {
    /// Caller-supplied identifier (typically `BookId`).
    pub key: K,
    /// The audiobook directory on disk. Treated as a subtree root.
    pub directory: PathBuf,
}

/// Geometry-only auto-pair. Pure function; no I/O.
///
/// `companion_path` is the absolute path of the companion file
/// (e.g. `.../author/series/book-1/notes.pdf`). `audiobooks` is
/// every candidate's directory; the function picks zero / one /
/// many based on which directories sit on the companion's
/// ancestor chain.
///
/// All paths must be **canonicalised** by the caller (the
/// scanner reads disk truth via `std::fs::canonicalize` before
/// calling). Pure-string ancestor matching avoids `..` traversal
/// surprises and means this function can be called inside a
/// hot loop without I/O.
#[must_use]
pub fn auto_pair<K: BookKey>(
    companion_path: &Path,
    audiobooks: &[AudiobookCandidate<K>],
) -> AutoPairResult<K> {
    let candidates: Vec<K> = audiobooks
        .iter()
        .filter(|c| is_ancestor_or_equal(&c.directory, companion_path))
        .map(|c| c.key.clone())
        .collect();
    let mut iter = candidates.into_iter();
    match (iter.next(), iter.next()) {
        (None, _) => AutoPairResult::Unpaired,
        (Some(only), None) => AutoPairResult::Paired(only),
        (Some(first), Some(second)) => {
            let mut all = Vec::with_capacity(2);
            all.push(first);
            all.push(second);
            all.extend(iter);
            AutoPairResult::Ambiguous(all)
        }
    }
}

/// `dir` is an ancestor of `path`, or they're equal. Strict
/// component-by-component prefix check — no string-prefix
/// surprises (so `/a/foo` doesn't ancestor-match `/a/foobar`).
#[must_use]
pub fn is_ancestor_or_equal(dir: &Path, path: &Path) -> bool {
    let dir_components: Vec<_> = dir.components().collect();
    let path_components: Vec<_> = path.components().collect();
    if dir_components.len() > path_components.len() {
        return false;
    }
    dir_components
        .iter()
        .zip(path_components.iter())
        .all(|(a, b)| a == b)
}

/// True when the companion's directory diverges from the
/// paired book's audio-files directory — drives the ❓
/// indicator per ADR-0043 § "Path-divergence ❓ heuristic".
///
/// The condition: the companion's parent dir is **not** equal
/// to and **not** a descendant of the audio-files dir. A
/// matching dir (or deeper) clears the ❓; anything else
/// surfaces it.
#[must_use]
pub fn path_diverges(companion_path: &Path, audio_files_dir: &Path) -> bool {
    let Some(companion_dir) = companion_path.parent() else {
        return true;
    };
    !is_ancestor_or_equal(audio_files_dir, companion_dir)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn candidate(key: u32, dir: &str) -> AudiobookCandidate<u32> {
        AudiobookCandidate {
            key,
            directory: PathBuf::from(dir),
        }
    }

    #[test]
    fn unpaired_when_no_audiobook_dir_is_an_ancestor() {
        let companion = PathBuf::from("/library/orphans/notes.pdf");
        let books = vec![
            candidate(1, "/library/sanderson/stormlight/book1"),
            candidate(2, "/library/king/dark-tower"),
        ];
        assert_eq!(auto_pair(&companion, &books), AutoPairResult::Unpaired);
    }

    #[test]
    fn paired_when_exactly_one_audiobook_dir_contains_companion() {
        let companion = PathBuf::from("/library/sanderson/stormlight/book1/notes.pdf");
        let books = vec![
            candidate(1, "/library/sanderson/stormlight/book1"),
            candidate(2, "/library/king/dark-tower"),
        ];
        assert_eq!(auto_pair(&companion, &books), AutoPairResult::Paired(1));
    }

    #[test]
    fn paired_when_companion_in_subdirectory_of_audiobook_dir() {
        // 2026-05-14 ADR refinement: subdirectories also pair.
        let companion = PathBuf::from("/library/sanderson/stormlight/book1/extras/maps.pdf");
        let books = vec![candidate(1, "/library/sanderson/stormlight/book1")];
        assert_eq!(auto_pair(&companion, &books), AutoPairResult::Paired(1));
    }

    #[test]
    fn ambiguous_when_multiple_audiobook_dirs_share_ancestor() {
        // companion sits in a shared series dir with multiple books
        let companion = PathBuf::from("/library/sanderson/stormlight/notes.pdf");
        let books = vec![
            candidate(1, "/library/sanderson/stormlight"),
            candidate(2, "/library/sanderson/stormlight/book1"),
            candidate(3, "/library/sanderson/stormlight/book2"),
        ];
        let result = auto_pair(&companion, &books);
        // Only book #1 (the series dir) ancestor-matches the
        // companion. Books 2 and 3 are descendants of the
        // companion's dir, not ancestors of it.
        assert_eq!(result, AutoPairResult::Paired(1));
    }

    #[test]
    fn ambiguous_when_two_audiobook_dirs_both_ancestor_match() {
        let companion = PathBuf::from("/library/notes.pdf");
        let books = vec![candidate(1, "/library"), candidate(2, "/library")];
        match auto_pair(&companion, &books) {
            AutoPairResult::Ambiguous(keys) => {
                assert_eq!(keys.len(), 2);
                assert!(keys.contains(&1));
                assert!(keys.contains(&2));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn component_match_does_not_string_prefix() {
        // `/a/foo` is NOT an ancestor of `/a/foobar/x.pdf` even
        // though the string `/a/foo` is a prefix of
        // `/a/foobar/x.pdf`.
        assert!(!is_ancestor_or_equal(
            Path::new("/a/foo"),
            Path::new("/a/foobar/x.pdf")
        ));
        // But it IS for `/a/foo/x.pdf`.
        assert!(is_ancestor_or_equal(
            Path::new("/a/foo"),
            Path::new("/a/foo/x.pdf")
        ));
    }

    #[test]
    fn path_diverges_for_sibling_dirs() {
        // companion in sibling dir → diverges
        let companion = PathBuf::from("/library/extras/notes.pdf");
        let audio_dir = PathBuf::from("/library/audio");
        assert!(path_diverges(&companion, &audio_dir));
    }

    #[test]
    fn path_does_not_diverge_when_companion_in_audio_dir() {
        let companion = PathBuf::from("/library/audio/notes.pdf");
        let audio_dir = PathBuf::from("/library/audio");
        assert!(!path_diverges(&companion, &audio_dir));
    }

    #[test]
    fn path_does_not_diverge_when_companion_in_subdir() {
        let companion = PathBuf::from("/library/audio/extras/notes.pdf");
        let audio_dir = PathBuf::from("/library/audio");
        assert!(!path_diverges(&companion, &audio_dir));
    }
}
