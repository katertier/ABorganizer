//! Banned identifier check.
//!
//! Words like `Manager`, `Service`, `Helper`, `Util`, `Factory`,
//! `Handler` are weasel words that obscure what the type actually
//! holds or does. Banned in struct/enum/trait names per
//! `docs/POLICIES.md`.
//!
//! `xtask: allow_macros` — this checker prints results to stderr.

use anyhow::Result;
use regex::Regex;

use crate::checks::walk;

const BANNED: &[&str] = &[
    "Manager", "Service", "Helper", "Util", "Utils", "Factory", "Handler",
];

/// Run the check.
pub(crate) fn run() -> Result<u32> {
    let root = workspace_root();
    let sources = walk::rust_sources(&root);
    let mut violations: u32 = 0;

    // Match `struct X`, `enum X`, `trait X` where `X` is an identifier.
    let item_re =
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]+\))?\s+)?(struct|enum|trait)\s+([A-Z][A-Za-z0-9_]*)")?;

    for path in sources {
        let content = std::fs::read_to_string(&path)?;
        for cap in item_re.captures_iter(&content) {
            let kind = &cap[1];
            let name = &cap[2];
            for banned in BANNED {
                if name.ends_with(banned) || name == *banned {
                    eprintln!(
                        "names: {} declares {kind} `{name}` containing banned suffix `{banned}`",
                        path.strip_prefix(&root).unwrap_or(&path).display()
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
