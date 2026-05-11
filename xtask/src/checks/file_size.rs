//! Enforce file-line-count cap.

// xtask: allow_macros — this checker prints results to stderr.

use anyhow::Result;

use crate::checks::walk;

/// Maximum lines per source file. Override per-file with a header
/// comment `// xtask: max_lines = NNN`.
pub(crate) const DEFAULT_MAX_LINES: usize = 500;

/// Run the check. Returns the number of violations.
pub(crate) fn run() -> Result<u32> {
    let root = workspace_root();
    let sources = walk::rust_sources(&root);
    let mut violations: u32 = 0;
    for path in sources {
        let content = std::fs::read_to_string(&path)?;
        let line_count = content.lines().count();
        let cap = override_cap(&content).unwrap_or(DEFAULT_MAX_LINES);
        if line_count > cap {
            eprintln!(
                "file_size: {} has {} lines (cap {})",
                path.strip_prefix(&root).unwrap_or(&path).display(),
                line_count,
                cap
            );
            violations += 1;
        }
    }
    Ok(violations)
}

fn override_cap(content: &str) -> Option<usize> {
    for line in content.lines().take(5) {
        if let Some(rest) = line.strip_prefix("// xtask: max_lines = ") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return Some(n);
            }
        }
    }
    None
}

fn workspace_root() -> std::path::PathBuf {
    std::env::var_os("CARGO_WORKSPACE_DIR").map_or_else(
        || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."),
        std::path::PathBuf::from,
    )
}
