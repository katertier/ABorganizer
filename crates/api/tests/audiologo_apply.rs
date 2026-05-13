// Integration test target: same lint-relaxation as the other
// integration tests in this workspace — `expect()` / `unwrap()`
// are setup idioms, `panic!()` is for "unexpected branch", and
// the `books.X` shorthand in doc comments is a column reference.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown,
    // `as i64` on small loop counters + array indices in fixture
    // helpers — these are bounded by literal arrays of < 10 elems.
    clippy::cast_possible_wrap
)]

//! Integration test for the chapter-shift maths in
//! `ab_api::audiologo_apply`. ADR-0024 § Chapter
//! recomputation.
//!
//! This is the "would have been planned from the start" piece
//! that slice 4A originally lacked. The chapter-shift behavior
//! is the most complex bit of audiologo apply: it has to
//! translate file-local cuts into book-cumulative chapter
//! offsets, which means correctly accounting for previously-
//! applied cuts on earlier files.
//!
//! Three scenarios pin the contract:
//!
//! 1. **Single intro on a single-file book** — baseline:
//!    chapters after the cut shift, chapters spanning the cut
//!    keep their start but shift their end, chapters entirely
//!    before are untouched.
//! 2. **Outro on the last file of a multi-file book** —
//!    cumulative offset is non-zero (sum of all preceding
//!    files' durations); chapters in the last file shift
//!    correctly relative to the book-cumulative time-base.
//! 3. **Sequential cuts on different files** — this is the
//!    bug-class that slice 4A's first draft missed: a cut on
//!    file 0 shifts the chapters; the cumulative offset for a
//!    subsequent cut on file 1 must SUBTRACT the file-0 cut to
//!    line up with the already-shifted chapter positions.
//!    Without that subtraction the second cut's shift targets
//!    the wrong rows.

use ab_api::audiologo_apply::{ApplyCutParams, apply_audiologo_cut};
use ab_core::tunables::DbTunables;
use ab_db::LibraryDb;
use tempfile::TempDir;

async fn fresh_db() -> (LibraryDb, TempDir) {
    let tmp = TempDir::new().expect("tmpdir");
    let lib = LibraryDb::open(&tmp.path().join("library.db"), &DbTunables::default())
        .await
        .expect("open library");
    (lib, tmp)
}

/// Insert a book with `n_files` files of equal `file_duration_ms`.
/// Returns `(book_id, file_ids)`. The `book_files.file_id`s are
/// sequential starting from 1 (file 0 in array = first inserted).
///
/// Uses runtime `sqlx::query()` (not macros) per the project's
/// "test-only queries stay runtime" rule from `CLAUDE.md` —
/// `cargo sqlx prepare` doesn't reach `#[cfg(test)]` cleanly.
async fn fixture_book(
    lib: &LibraryDb,
    title: &str,
    n_files: usize,
    file_duration_ms: i64,
) -> (i64, Vec<i64>) {
    let total_duration = file_duration_ms * (n_files as i64);
    let book_id: i64 = sqlx::query_scalar(
        "INSERT INTO books (title, duration_ms, raw_duration_ms) \
         VALUES (?, ?, ?) RETURNING book_id",
    )
    .bind(title)
    .bind(total_duration)
    .bind(total_duration)
    .fetch_one(lib.pool())
    .await
    .expect("insert book");

    let mut file_ids = Vec::with_capacity(n_files);
    for i in 0..n_files {
        let path = format!("/test/{title}/{i}.m4b");
        let file_id: i64 = sqlx::query_scalar(
            "INSERT INTO book_files (book_id, file_path, duration_ms) \
             VALUES (?, ?, ?) RETURNING file_id",
        )
        .bind(book_id)
        .bind(&path)
        .bind(file_duration_ms)
        .fetch_one(lib.pool())
        .await
        .expect("insert book_file");
        file_ids.push(file_id);
    }
    (book_id, file_ids)
}

/// Insert chapters at book-cumulative offsets. `chapters` is a
/// list of `(start_ms, end_ms, title)` tuples.
async fn fixture_chapters(lib: &LibraryDb, book_id: i64, chapters: &[(i64, i64, &str)]) {
    for (idx, (start_ms, end_ms, title)) in chapters.iter().enumerate() {
        let idx_i64 = idx as i64;
        sqlx::query(
            "INSERT INTO chapters (book_id, idx, start_ms, end_ms, title, source) \
             VALUES (?, ?, ?, ?, ?, 'test')",
        )
        .bind(book_id)
        .bind(idx_i64)
        .bind(*start_ms)
        .bind(*end_ms)
        .bind(*title)
        .execute(lib.pool())
        .await
        .expect("insert chapter");
    }
}

/// Read `(start_ms, end_ms)` for every chapter in the book,
/// ordered by `idx`.
async fn read_chapter_offsets(lib: &LibraryDb, book_id: i64) -> Vec<(i64, i64)> {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT start_ms, end_ms FROM chapters WHERE book_id = ? ORDER BY idx",
    )
    .bind(book_id)
    .fetch_all(lib.pool())
    .await
    .expect("read chapters")
}

/// Read `books.duration_ms` + `books.audiologo_status`.
async fn read_book_state(lib: &LibraryDb, book_id: i64) -> (Option<i64>, String) {
    sqlx::query_as::<_, (Option<i64>, String)>(
        "SELECT duration_ms, audiologo_status FROM books WHERE book_id = ?",
    )
    .bind(book_id)
    .fetch_one(lib.pool())
    .await
    .expect("read book")
}

#[tokio::test]
async fn single_file_intro_shifts_chapters_correctly() {
    let (lib, _tmp) = fresh_db().await;

    // One file, 60 seconds long. Three chapters laid out:
    //   ch0: [0, 20_000)        — entirely before the trim
    //                             (NOTE: starts at 0; the trim
    //                              is at [10_000, 15_000] which
    //                              is *inside* ch0, so ch0 ends
    //                              up "spanning" the trim).
    //   ch1: [20_000, 40_000)   — entirely after the trim
    //   ch2: [40_000, 60_000)   — entirely after the trim
    let (book_id, files) = fixture_book(&lib, "single", 1, 60_000).await;
    fixture_chapters(
        &lib,
        book_id,
        &[
            (0, 20_000, "ch0"),
            (20_000, 40_000, "ch1"),
            (40_000, 60_000, "ch2"),
        ],
    )
    .await;

    // Cut: file-local [10_000, 15_000], padding 0 → cut_ms = 5_000.
    let outcome = apply_audiologo_cut(
        lib.pool(),
        ApplyCutParams {
            book_id,
            file_id: files[0],
            kind: "intro",
            jingle_start_ms: 10_000,
            jingle_end_ms: 15_000,
            padding_ms: Some(0),
            method: "manual",
            audiologo_id: None,
            confidence: 1.0,
        },
    )
    .await
    .expect("apply succeeds");

    // ch0 spans the trim → only end_ms shifts.
    // ch1 and ch2 are entirely after → both endpoints shift.
    let offsets = read_chapter_offsets(&lib, book_id).await;
    assert_eq!(
        offsets,
        vec![
            (0, 15_000),      // ch0: start unchanged, end -5000
            (15_000, 35_000), // ch1: -5000
            (35_000, 55_000), // ch2: -5000
        ],
        "chapter shift mismatch",
    );

    // chapters_shifted counts all touched rows.
    assert_eq!(outcome.chapters_shifted, 3, "expected 3 rows touched");

    // duration_ms = 60_000 - 5_000 = 55_000; status = applied.
    let (duration, status) = read_book_state(&lib, book_id).await;
    assert_eq!(duration, Some(55_000));
    assert_eq!(status, "applied");
}

#[tokio::test]
async fn outro_shift_in_multi_file_book_uses_correct_cumulative_offset() {
    let (lib, _tmp) = fresh_db().await;

    // Two files, 60s each. Total duration 120_000.
    // Chapters in book-cumulative time:
    //   ch0: [0, 30_000)
    //   ch1: [30_000, 60_000)
    //   ch2: [60_000, 90_000)
    //   ch3: [90_000, 120_000)
    //
    // Apply an outro on file[1] at file-local [55_000, 60_000]
    // (padding 0, cut_ms = 5_000). In book-cumulative time
    // that's [115_000, 120_000]. ch3 spans the trim → end shifts.
    let (book_id, files) = fixture_book(&lib, "multi", 2, 60_000).await;
    fixture_chapters(
        &lib,
        book_id,
        &[
            (0, 30_000, "ch0"),
            (30_000, 60_000, "ch1"),
            (60_000, 90_000, "ch2"),
            (90_000, 120_000, "ch3"),
        ],
    )
    .await;

    apply_audiologo_cut(
        lib.pool(),
        ApplyCutParams {
            book_id,
            file_id: files[1],
            kind: "outro",
            jingle_start_ms: 55_000,
            jingle_end_ms: 60_000,
            padding_ms: Some(0),
            method: "manual",
            audiologo_id: None,
            confidence: 1.0,
        },
    )
    .await
    .expect("apply succeeds");

    let offsets = read_chapter_offsets(&lib, book_id).await;
    assert_eq!(
        offsets,
        vec![
            (0, 30_000),       // ch0: untouched (entirely before)
            (30_000, 60_000),  // ch1: untouched (entirely before)
            (60_000, 90_000),  // ch2: untouched (entirely before)
            (90_000, 115_000), // ch3: spans → end_ms -5000
        ],
        "outro shift in multi-file book wrong",
    );

    let (duration, _) = read_book_state(&lib, book_id).await;
    assert_eq!(duration, Some(115_000));
}

#[tokio::test]
async fn sequential_cuts_on_different_files_account_for_prior_shifts() {
    // This is the bug-class regression test (ADR-0024
    // cumulative-offset accounting). Without subtracting the
    // prior cut from the cumulative offset of subsequent cuts,
    // the second cut's `jingle_*_book_ms` would point at raw
    // positions while the chapter rows are already in shifted
    // positions; the resulting comparison would mis-target.
    let (lib, _tmp) = fresh_db().await;

    // Two files, 60s each. Chapters at book-cumulative offsets:
    //   ch0: [0, 30_000)       — file 0
    //   ch1: [30_000, 60_000)  — file 0
    //   ch2: [60_000, 90_000)  — file 1
    //   ch3: [90_000, 120_000) — file 1
    let (book_id, files) = fixture_book(&lib, "seq", 2, 60_000).await;
    fixture_chapters(
        &lib,
        book_id,
        &[
            (0, 30_000, "ch0"),
            (30_000, 60_000, "ch1"),
            (60_000, 90_000, "ch2"),
            (90_000, 120_000, "ch3"),
        ],
    )
    .await;

    // Cut 1: intro on file 0 at file-local [0, 4_000],
    // padding 0 → cut_ms = 4_000.
    // In book-cumulative: [0, 4_000].
    // ch0 spans (start 0 < trim_end 4_000; end 30_000 >
    // trim_start 0) → end shifts to 26_000.
    // ch1/2/3 entirely after → both endpoints shift -4000.
    apply_audiologo_cut(
        lib.pool(),
        ApplyCutParams {
            book_id,
            file_id: files[0],
            kind: "intro",
            jingle_start_ms: 0,
            jingle_end_ms: 4_000,
            padding_ms: Some(0),
            method: "manual",
            audiologo_id: None,
            confidence: 1.0,
        },
    )
    .await
    .expect("apply 1 succeeds");

    // After cut 1, the chapters should be:
    //   ch0: [0, 26_000)        (spans; end -4000)
    //   ch1: [26_000, 56_000)   (after; -4000)
    //   ch2: [56_000, 86_000)   (after; -4000)
    //   ch3: [86_000, 116_000)  (after; -4000)
    let after_first = read_chapter_offsets(&lib, book_id).await;
    assert_eq!(
        after_first,
        vec![
            (0, 26_000),
            (26_000, 56_000),
            (56_000, 86_000),
            (86_000, 116_000),
        ],
        "first cut: state before the second cut applies",
    );

    // Cut 2: intro on file 1 at file-local [0, 3_000],
    // padding 0 → cut_ms = 3_000.
    //
    // Without cumulative-offset accounting for the prior cut,
    // we'd compute jingle_end_book_ms = 60_000 + 3_000 =
    // 63_000 (raw cumulative for file 1's start + 3000).
    // BUT chapter rows have already been shifted: ch2 is
    // now at [56_000, 86_000), not [60_000, 90_000). The
    // "entirely after" comparison `start_ms >= 63_000` would
    // miss ch2 entirely (its current start_ms is 56_000) and
    // wrongly leave it unshifted, while shifting ch3 by 3000.
    //
    // With the correct accounting:
    //   raw_sum(files < file_1) = 60_000
    //   applied_cuts(files < file_1) = 4_000 (the prior cut)
    //   cumulative_before = 60_000 - 4_000 = 56_000
    //   jingle_start_book_ms = 56_000 + 0     = 56_000
    //   jingle_end_book_ms   = 56_000 + 3_000 = 59_000
    //
    // ch2's start_ms (56_000) is < 59_000 AND its end_ms
    // (86_000) > 56_000 → ch2 SPANS the trim → only ch2's
    // end_ms shifts. ch3 is entirely after (start 86_000 >=
    // 59_000) → both shift.
    apply_audiologo_cut(
        lib.pool(),
        ApplyCutParams {
            book_id,
            file_id: files[1],
            kind: "intro",
            jingle_start_ms: 0,
            jingle_end_ms: 3_000,
            padding_ms: Some(0),
            method: "manual",
            audiologo_id: None,
            confidence: 1.0,
        },
    )
    .await
    .expect("apply 2 succeeds");

    let final_offsets = read_chapter_offsets(&lib, book_id).await;
    assert_eq!(
        final_offsets,
        vec![
            (0, 26_000),       // ch0: untouched by cut 2
            (26_000, 56_000),  // ch1: untouched by cut 2
            (56_000, 83_000),  // ch2: spans → end -3000
            (83_000, 113_000), // ch3: after → both -3000
        ],
        "sequential-cut accounting wrong — \
         cumulative-offset bug regression",
    );

    // duration_ms should now be 120_000 - 4_000 - 3_000 = 113_000.
    let (duration, _) = read_book_state(&lib, book_id).await;
    assert_eq!(duration, Some(113_000));
}

#[tokio::test]
async fn chapter_entirely_before_the_trim_is_untouched() {
    let (lib, _tmp) = fresh_db().await;

    // Three chapters; one is entirely before the cut, one
    // spans it, one is entirely after.
    let (book_id, files) = fixture_book(&lib, "before", 1, 60_000).await;
    fixture_chapters(
        &lib,
        book_id,
        &[
            (0, 5_000, "before"),     // ends at 5_000 — entirely before [10_000, 15_000]
            (5_000, 20_000, "spans"), // spans the trim
            (20_000, 60_000, "after"),
        ],
    )
    .await;

    apply_audiologo_cut(
        lib.pool(),
        ApplyCutParams {
            book_id,
            file_id: files[0],
            kind: "intro",
            jingle_start_ms: 10_000,
            jingle_end_ms: 15_000,
            padding_ms: Some(0),
            method: "manual",
            audiologo_id: None,
            confidence: 1.0,
        },
    )
    .await
    .expect("apply succeeds");

    let offsets = read_chapter_offsets(&lib, book_id).await;
    assert_eq!(
        offsets,
        vec![
            (0, 5_000),       // entirely before — untouched
            (5_000, 15_000),  // spans → end -5000
            (15_000, 55_000), // after → both -5000
        ],
    );
}

#[tokio::test]
async fn padding_clamps_cut_amount() {
    let (lib, _tmp) = fresh_db().await;

    // Padding > jingle range → cut_ms clamps to 0; chapters
    // untouched; row inserts at status=applied; duration_ms
    // unchanged. This is the "padding swallowed the jingle"
    // edge case — handled gracefully by the .max(0) clamp.
    let (book_id, files) = fixture_book(&lib, "padding", 1, 60_000).await;
    fixture_chapters(
        &lib,
        book_id,
        &[(0, 30_000, "ch0"), (30_000, 60_000, "ch1")],
    )
    .await;

    let before = read_chapter_offsets(&lib, book_id).await;
    let outcome = apply_audiologo_cut(
        lib.pool(),
        ApplyCutParams {
            book_id,
            file_id: files[0],
            kind: "intro",
            jingle_start_ms: 0,
            jingle_end_ms: 1_000,
            padding_ms: Some(2_000), // > jingle range
            method: "manual",
            audiologo_id: None,
            confidence: 1.0,
        },
    )
    .await
    .expect("apply succeeds even with over-padding");

    // chapters untouched by a 0-effective cut.
    assert_eq!(read_chapter_offsets(&lib, book_id).await, before);
    // duration_ms unchanged.
    let (duration, status) = read_book_state(&lib, book_id).await;
    assert_eq!(duration, Some(60_000));
    // status still flipped (row inserted at applied).
    assert_eq!(status, "applied");
    assert!(outcome.row_id > 0);
}
