//! Standard macOS paths derived from the app name (see
//! `crate::build_info`). All app-managed locations resolve here so
//! `xtask check` can scan source files for hardcoded directory strings.

use std::path::PathBuf;

use crate::build_info;

/// `~/Library/Application Support/<DisplayName>` — canonical persistent
/// data root. Honors Apple's File System Programming Guide.
pub fn app_support_dir() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Application Support")
        .join(build_info::DISPLAY_NAME)
}

/// `~/Library/Caches/<DisplayName>` — rebuildable caches (Tantivy
/// index, downloaded covers, decoded image variants). System-managed:
/// may be cleared on low-space conditions.
pub fn cache_dir() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Caches")
        .join(build_info::DISPLAY_NAME)
}

/// `~/Library/Logs/<DisplayName>` — file logs when enabled (off by
/// default; OSLog is always on).
pub fn logs_dir() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Logs")
        .join(build_info::DISPLAY_NAME)
}

/// Canonical OSLog subsystem for a sub-program. Compose with the
/// program name (e.g. `"daemon"`, `"menubar"`, `"cli"`).
///
/// Example: `osklog_subsystem("daemon")` →
/// `"io.github.katertier.aborganizer.daemon"`.
pub fn oslog_subsystem(program: &str) -> String {
    format!("{}.{}", build_info::BUNDLE_ID_BASE, program)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}
