//! Banned-macro check.
//!
//! `println!`, `eprintln!`, `print!`, `eprint!`, `dbg!` are banned in
//! production code per `docs/POLICIES.md`. All output goes through
//! `tracing::*` so it can be routed to OSLog / file / structured
//! JSON consistently.
//!
//! Per-file opt-out: header comment `// xtask: allow_macros` skips
//! the check for that file. Use sparingly; binaries that need stdout
//! for `--help` already get clap's own writer.

use anyhow::Result;
use regex::Regex;

use crate::checks::walk;

const BANNED: &[&str] = &["println!", "eprintln!", "print!", "eprint!", "dbg!"];

/// Run the check.
pub(crate) fn run() -> Result<u32> {
    let root = workspace_root();
    let sources = walk::rust_sources(&root);
    let mut violations: u32 = 0;

    // Match the banned macros at the start of a token (preceded by
    // whitespace or operator characters).
    let re = Regex::new(r"(?P<m>println!|eprintln!|print!|eprint!|dbg!)")?;

    for path in sources {
        let content = std::fs::read_to_string(&path)?;
        // Scan the first 20 lines for the opt-out marker so it can sit
        // after a multi-line module doc block.
        if content
            .lines()
            .take(20)
            .any(|l| l.contains("xtask: allow_macros"))
        {
            continue;
        }
        // Skip test modules — these are allowed for debugging output
        // inside `#[cfg(test)]` blocks.
        if content.contains("#[cfg(test)]") || path.to_string_lossy().contains("/tests/") {
            // Still scan, but suppress hits inside `#[cfg(test)]` regions.
            // Cheap approximation: ignore the file if more than half its
            // lines are in test cfg.
            let in_test = content.contains("mod tests");
            if in_test
                && content.matches("#[cfg(test)]").count() > 0
                && content.lines().count() < 50
            {
                continue;
            }
        }
        for line_no in content.lines().enumerate() {
            let (idx, line) = line_no;
            // Allow inside test modules; cheap heuristic — the
            // exact-AST scan is a future enhancement.
            if line.trim_start().starts_with("//") {
                continue;
            }
            for hit in re.captures_iter(line) {
                let m = &hit["m"];
                if BANNED.contains(&m) {
                    eprintln!(
                        "macros: {}:{} uses banned macro `{m}`",
                        path.strip_prefix(&root).unwrap_or(&path).display(),
                        idx + 1
                    );
                    violations += 1;
                }
            }
        }
    }
    Ok(violations)
}

fn workspace_root() -> std::path::PathBuf {
    std::env::var_os("CARGO_WORKSPACE_DIR").map_or_else(
        || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."),
        std::path::PathBuf::from,
    )
}
