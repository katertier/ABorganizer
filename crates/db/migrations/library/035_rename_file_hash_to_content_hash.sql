-- Rename `book_files.file_hash` → `book_files.content_hash` for naming
-- consistency with `book_companions.content_hash`. The hash is BLAKE3 over
-- (size + mtime + first 4 KiB) per the existing scan/lib.rs contract; the
-- semantic is unchanged — only the column name moves.
--
-- Part of the schema-as-if-planned-from-day-one slice (2026-05-15
-- retrospective, item #4). The pair of column names was a historical accident
-- of two crates landing months apart; harmonising now avoids new code
-- continuing the drift.
--
-- SQLite 3.25+ supports `ALTER TABLE … RENAME COLUMN`. Both the
-- libsqlite3-sys bundled with sqlx 0.8 and the Homebrew sqlite used by
-- scripts/sqlx-prepare.sh are well past that version.

ALTER TABLE book_files RENAME COLUMN file_hash TO content_hash;
