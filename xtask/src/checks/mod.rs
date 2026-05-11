//! Aggregated check runner.

// xtask: allow_macros — this aggregator prints results to stderr.

pub(crate) mod brand;
pub(crate) mod macros;
pub(crate) mod names;
pub(crate) mod walk;

use anyhow::Result;

// Note: the previous `file_size` check (default 500-line cap) was
// dropped. Size correlates poorly with code quality — see
// `~/dev/ABorganizer-docs/POLICIES.md` § "File size" for the
// rationale. The structural checks below (banned identifiers,
// banned macros, brand discipline) target the real failure modes
// behind convoluted code in the predecessor codebase.

/// Run every registered check. Return total count of issues.
pub(crate) fn run_all() -> Result<u32> {
    let mut total = 0;
    total += names::run()?;
    total += macros::run()?;
    total += brand::run()?;
    if total == 0 {
        eprintln!("xtask check: all checks pass");
    }
    Ok(total)
}
