//! Aggregated check runner.

// xtask: allow_macros — this aggregator prints results to stderr.

pub(crate) mod brand;
pub(crate) mod file_size;
pub(crate) mod macros;
pub(crate) mod names;
pub(crate) mod walk;

use anyhow::Result;

/// Run every registered check. Return total count of issues.
pub(crate) fn run_all() -> Result<u32> {
    let mut total = 0;
    total += file_size::run()?;
    total += names::run()?;
    total += macros::run()?;
    total += brand::run()?;
    if total == 0 {
        eprintln!("xtask check: all checks pass");
    }
    Ok(total)
}
