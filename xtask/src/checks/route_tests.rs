//! Route-test coverage check.
//!
//! `xtask: allow_macros` — this checker prints results to stderr.
//!
//! For every `Router::route(LIT, …)` call in a crate, require that
//! some test in the same crate exercises a URI the route would match.
//!
//! ## Why
//!
//! The axum 0.7 router parses `{id}` as a literal path segment, not a
//! placeholder — a typo there silently 404s every real request.
//! Compiles fine, types match, no warning. The only way to catch
//! the bug is to *actually exercise the route from a test*. Slice C1
//! caught the bug in the shelf router exactly that way; this lint
//! makes the requirement structural.
//!
//! ## How
//!
//! 1. Per crate (everything under `crates/<name>/` or `bins/<name>/`):
//!    - Collect every `.route("…", …)` string literal from non-test
//!      `.rs` files — the **routes** to cover.
//!    - Collect every string literal starting with `/` from test
//!      files (`#[cfg(test)]` modules, files ending in
//!      `integration_tests.rs`, and `tests/**`) — the **test URIs**.
//! 2. Convert each route into a regex (`:param` / `{param}` →
//!    `[^/]+`) and check that some test URI matches.
//! 3. Routes with no matching test URI are violations.
//!
//! ## Exemptions
//!
//! A crate listed in [`CRATE_EXEMPTIONS`] is skipped wholesale.
//! Each entry needs a one-line rationale pointing at the slice that
//! tracks closing the coverage gap. New entries are paperwork the
//! reviewer can challenge; the default is enforcement.
//!
//! ## Tradeoffs
//!
//! - Loose URI extraction. We accept any `"/..."` literal in a test
//!   file as a candidate URI rather than only those nested inside
//!   `.uri(…)`. Tests pass URIs through helper functions and
//!   `format!()` enough that strict extraction misses real coverage.
//!   False positives mark routes covered when they aren't — but in
//!   practice path-shaped literals in test files almost always *are*
//!   URIs, and the alternative (tight `.uri(LIT)` matching) misses
//!   the helper pattern used by the shelf integration tests today.
//!
//! - Crate-level granularity. We don't try to associate a specific
//!   test with a specific router; both must just exist in the same
//!   crate. Multi-router crates would need finer scope; today there
//!   is one router per crate that has routes at all.

// xtask: allow_macros — this checker prints results to stderr.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use regex::Regex;

use crate::checks::walk;

/// Crates that opt out of route-test-coverage enforcement.
///
/// Each entry: `(crate-name, rationale)`. The rationale should
/// reference the slice / task tracking the gap so the entry is
/// removable. New entries need a code-review challenge — the
/// default is enforcement.
const CRATE_EXEMPTIONS: &[(&str, &str)] = &[(
    "api",
    "router predates integration-test scaffolding; ~30 routes \
         need harness work tracked as a follow-up to #83",
)];

/// Run the check.
pub(crate) fn run() -> Result<u32> {
    let root = workspace_root();
    let sources = walk::rust_sources(&root);

    // Captures the path literal inside `.route("…", …)`.
    //
    // We deliberately don't try to handle string concat or `concat!()`
    // — every route in the workspace today is a bare literal, and
    // forcing that idiom is itself useful discipline.
    let route_re = Regex::new(r#"\.route\(\s*"([^"]+)""#)?;

    // Captures any path-like string literal (starts with `/`).
    // Used only against test files.
    let path_lit_re = Regex::new(r#""(/[^"\\]*)""#)?;

    let mut by_crate: BTreeMap<String, CrateBundle> = BTreeMap::new();
    for path in &sources {
        let Some(crate_name) = crate_of(path, &root) else {
            continue;
        };
        // Skip xtask itself — it has no routes and pulling its own
        // regex literals into the URI bag is noise.
        if crate_name == "xtask" {
            continue;
        }
        let content = std::fs::read_to_string(path)?;
        let bundle = by_crate.entry(crate_name).or_default();

        if is_test_file(path, &content) {
            for cap in path_lit_re.captures_iter(&content) {
                bundle.test_uris.insert(cap[1].to_string());
            }
        } else {
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            for cap in route_re.captures_iter(&content) {
                bundle.routes.insert((cap[1].to_string(), rel.clone()));
            }
            // A non-test file can still contain inline `#[cfg(test)] mod`
            // tests further down the file; harvest URIs from those too.
            // Cheap heuristic: if the file mentions `#[cfg(test)]`,
            // pass it through the path-literal regex as well.
            if content.contains("#[cfg(test)]") {
                for cap in path_lit_re.captures_iter(&content) {
                    bundle.test_uris.insert(cap[1].to_string());
                }
            }
        }
    }

    let mut violations = 0u32;
    for (crate_name, bundle) in &by_crate {
        if bundle.routes.is_empty() {
            continue;
        }
        if let Some((_, why)) = CRATE_EXEMPTIONS
            .iter()
            .find(|(name, _)| *name == crate_name.as_str())
        {
            eprintln!("route-test-coverage: skipping `{crate_name}` — {why}");
            continue;
        }

        for (route, src_file) in &bundle.routes {
            let pattern = route_to_regex(route)?;
            let covered = bundle.test_uris.iter().any(|uri| pattern.is_match(uri));
            if !covered {
                eprintln!(
                    "route-test-coverage: {src_file} declares route `{route}` \
                     with no matching test URI in crate `{crate_name}`"
                );
                violations += 1;
            }
        }
    }
    Ok(violations)
}

#[derive(Default)]
struct CrateBundle {
    /// `(route-literal, source-file-relpath)` — keyed so the same
    /// route declared twice in different files only fires once.
    routes: BTreeSet<(String, String)>,
    /// Path-literals harvested from test files.
    test_uris: BTreeSet<String>,
}

/// Returns the crate name (path segment after `crates/` or `bins/`)
/// the given file lives inside, or `None` for workspace-root files.
fn crate_of(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut comps = rel.components();
    let first = comps.next()?.as_os_str().to_str()?.to_owned();
    let second = comps.next()?.as_os_str().to_str()?.to_owned();
    match first.as_str() {
        "crates" | "bins" => Some(second),
        _ => None,
    }
}

/// `true` if the file is a test surface: a `tests/` separate target,
/// a file conventionally named `integration_tests.rs`, or a `.rs`
/// file whose content includes any test attribute.
fn is_test_file(path: &Path, content: &str) -> bool {
    let s = path.to_string_lossy();
    if s.contains("/tests/") {
        return true;
    }
    if s.ends_with("integration_tests.rs") || s.ends_with("integration_test.rs") {
        return true;
    }
    // Inline-test files: detected via attributes. Note we also
    // independently re-scan `#[cfg(test)]`-containing non-test files
    // up in `run()` so inline tests still contribute URIs even when
    // the file is primarily production source.
    content.contains("#[tokio::test]") || content.contains("#[test]")
}

/// Convert an axum route literal to a regex matching real URI
/// strings. Accepts both axum 0.7 (`:param`) and axum 0.8
/// (`{param}`) placeholder syntax so the lint survives the
/// upcoming migration (task #84) without churn.
fn route_to_regex(route: &str) -> Result<Regex> {
    let mut pat = String::from("^");
    let mut chars = route.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ':' => {
                // Consume the placeholder name up to `/` or end-of-string.
                while let Some(&nc) = chars.peek() {
                    if nc == '/' {
                        break;
                    }
                    chars.next();
                }
                pat.push_str("[^/]+");
            }
            '{' => {
                // Consume up to and including the matching `}`.
                for nc in chars.by_ref() {
                    if nc == '}' {
                        break;
                    }
                }
                pat.push_str("[^/]+");
            }
            // Regex metacharacters that could appear in a path literal.
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '|' | '\\' | '$' | '^' => {
                pat.push('\\');
                pat.push(c);
            }
            _ => pat.push(c),
        }
    }
    pat.push('$');
    Ok(Regex::new(&pat)?)
}

fn workspace_root() -> PathBuf {
    std::env::var_os("CARGO_WORKSPACE_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."),
        PathBuf::from,
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unit tests assert behaviour over edge cases; .unwrap is the right shape here"
)]
mod tests {
    use super::*;

    #[test]
    fn route_to_regex_handles_static_path() {
        let r = route_to_regex("/healthcheck").unwrap();
        assert!(r.is_match("/healthcheck"));
        assert!(!r.is_match("/healthcheck/x"));
        assert!(!r.is_match("/health"));
    }

    #[test]
    fn route_to_regex_handles_axum07_param() {
        let r = route_to_regex("/api/items/:id").unwrap();
        assert!(r.is_match("/api/items/abc"));
        assert!(r.is_match("/api/items/123-456"));
        // Two segments past the colon — should not match.
        assert!(!r.is_match("/api/items/abc/extra"));
    }

    #[test]
    fn route_to_regex_handles_axum08_param() {
        let r = route_to_regex("/api/items/{id}").unwrap();
        assert!(r.is_match("/api/items/abc"));
        assert!(r.is_match("/api/items/{book_id}"));
        assert!(!r.is_match("/api/items/abc/extra"));
    }

    #[test]
    fn route_to_regex_multiple_params() {
        let r = route_to_regex("/api/items/:id/file/:ino").unwrap();
        assert!(r.is_match("/api/items/abc/file/123"));
        assert!(r.is_match("/api/items/{book_id}/file/{file_id}"));
        assert!(!r.is_match("/api/items/abc/file/123/extra"));
    }

    #[test]
    fn route_to_regex_escapes_dots() {
        // Hypothetical route with a literal dot; the regex must
        // treat it as a literal, not as a wildcard.
        let r = route_to_regex("/static/app.js").unwrap();
        assert!(r.is_match("/static/app.js"));
        assert!(!r.is_match("/static/appXjs"));
    }
}
