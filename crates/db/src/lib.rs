//! Persistence layer.
//!
//! Two SQLite databases:
//!
//! * **`library.db`** — canonical, persistent, user-owned (books,
//!   authors, narrators, series, audiologo fingerprints, playlists,
//!   sessions, provenance audit trail).
//! * **`ephemeral.db`** — restartable, throwable on crash (job queue,
//!   pipeline progress, rate-limit state, metrics).
//!
//! Both use WAL mode. The library DB uses `synchronous=NORMAL`; the
//! ephemeral DB uses `synchronous=OFF` since its data is by definition
//! restartable.
//!
//! Migrations live in `migrations/library/` and `migrations/ephemeral/`
//! under this crate. They run automatically on first connection.

#![allow(missing_docs)] // scaffold; will be tightened as queries land

pub mod book_file_refs;
pub mod ephemeral;
pub mod library;
pub mod migrations;

pub use ephemeral::EphemeralDb;
pub use library::LibraryDb;
