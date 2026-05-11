//! Filesystem walker shared by every check.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// Yields every `.rs` file inside the workspace, except those under
/// `target/`, `node_modules/`, hidden directories, or the `xtask`
/// crate itself (xtask scans others, not itself).
pub(crate) fn rust_sources(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.')
                && name != "target"
                && name != "node_modules"
                && name != "DerivedData"
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
        .map(|e| e.path().to_path_buf())
        .collect()
}
