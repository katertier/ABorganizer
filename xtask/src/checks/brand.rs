//! Hardcoded app-name / bundle-ID scan.
//!
//! Source files must reference `ab_core::build_info::*` constants
//! rather than hardcoding `"ABorganizer"`, `"aborg"`, the bundle ID
//! prefix, etc. Doc-comments are exempt (they're prose).

// xtask: allow_macros — this checker prints results to stderr.

use anyhow::Result;
use regex::Regex;

use crate::checks::walk;

const FORBIDDEN_LITERALS: &[&str] = &[
    "\"ABorganizer\"",
    "\"aborganizer\"",
    "\"aborg-daemon\"",
    "\"io.github.katertier.aborganizer\"",
];

/// Run the check.
pub(crate) fn run() -> Result<u32> {
    let root = workspace_root();
    let sources = walk::rust_sources(&root);
    let mut violations: u32 = 0;
    let doc_re = Regex::new(r"^\s*//[/!]?")?;

    for path in &sources {
        // The build script + the constants themselves are exempt.
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        if rel.ends_with("/build.rs")
            || rel.contains("crates/core/build.rs")
            || rel.contains("xtask/")
        {
            continue;
        }

        let content = std::fs::read_to_string(path)?;
        for (idx, line) in content.lines().enumerate() {
            if doc_re.is_match(line) {
                continue;
            }
            for lit in FORBIDDEN_LITERALS {
                if line.contains(lit) {
                    eprintln!(
                        "brand: {}:{} hardcodes {lit} — use ab_core::build_info instead",
                        rel,
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
