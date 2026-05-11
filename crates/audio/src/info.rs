//! Lightweight audio file probes. No FFI — pure Rust.

use std::path::Path;

use ab_core::Result;

/// Best-effort total duration in milliseconds.
/// Returns `Ok(None)` when the format isn't recognised.
///
/// # Errors
///
/// Returns [`ab_core::Error::Io`] on I/O failures.
#[allow(clippy::missing_const_for_fn)]
pub fn probe_duration_ms(_file: &Path) -> Result<Option<u64>> {
    // TODO: wire `lofty::probe` once the audio crate is wired up.
    Ok(None)
}
