//! `statvfs` shim for the cleanup loop (slice H.2.2, ADR-0025).
//!
//! [`ab_pipeline::cleanup::run_cleanup_loop`] accepts an
//! `Arc<dyn Fn() -> (u64, u64) + Send + Sync>` rather than depending
//! on a syscall directly — keeps the pipeline crate free of any
//! platform-specific FS deps and lets tests inject any
//! `(free_bytes, total_bytes)` they want. This module is where the
//! daemon turns "the path the library DB lives in" into a real
//! `statvfs(2)` call.
//!
//! Sentinel: `(u64::MAX, u64::MAX)` is "no pressure detected". The
//! pressure ratchet in [`ab_core::cleanup::compute_age_seconds`]
//! treats it as "free space is infinite," so the baseline age applies
//! and no tier triggers. The caller's logs surface the underlying
//! failure on the first miss so the operator can investigate.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use nix::sys::statvfs::statvfs;

/// Build the `disk_free` closure
/// [`ab_pipeline::cleanup::CleanupLoopCtx`] expects.
///
/// The closure stat-s `path` on every call (the cleanup loop's tick
/// rate is minutes-to-hours; the syscall cost is irrelevant). A
/// failed `statvfs` is logged at WARN and the sentinel
/// `(u64::MAX, u64::MAX)` is returned so the pressure ratchet
/// gracefully falls back to the baseline age.
pub(crate) fn disk_free_for(path: &Path) -> Arc<dyn Fn() -> (u64, u64) + Send + Sync> {
    let path: PathBuf = path.to_path_buf();
    Arc::new(move || statvfs_call(&path))
}

/// Same shim as [`disk_free_for`] but path-parameterised — used by
/// `ab_api::doctor::DiskPressureCheck` to stat each `library_roots`
/// row at check time. Closes over no path; the caller passes one
/// at each invocation.
pub(crate) fn disk_free_any() -> ab_api::doctor::DiskFreeFn {
    Arc::new(statvfs_call)
}

fn statvfs_call(path: &Path) -> (u64, u64) {
    match statvfs(path) {
        Ok(stat) => {
            // `blocks_available` is the non-superuser free count;
            // `blocks` is the total. Multiply by `fragment_size` for
            // bytes. APFS reports `fragment_size == block_size == 4096`;
            // ext4 / xfs / btrfs use the same convention.
            //
            // On macOS aarch64 (our deployment target) `c_ulong` and
            // `fsblkcnt_t` are both already `u64`, so the platform
            // types arrive ready to multiply. The `from`-style
            // conversion below short-circuits cleanly without
            // tripping `clippy::useless_conversion` because nix's
            // type aliases expand differently per target.
            let block: u64 = stat.fragment_size();
            let free_blocks: u64 = stat.blocks_available().into();
            let total_blocks: u64 = stat.blocks().into();
            let free = free_blocks.saturating_mul(block);
            let total = total_blocks.saturating_mul(block);
            (free, total)
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "daemon.cleanup.statvfs_failed"
            );
            (u64::MAX, u64::MAX)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_returns_plausible_numbers_for_tmp() {
        // /tmp exists on every supported platform; we just check
        // the closure returns *something* non-sentinel.
        let f = disk_free_for(Path::new("/tmp"));
        let (free, total) = f();
        assert!(total > 0, "total bytes should be > 0 for /tmp");
        assert!(
            free <= total,
            "free ({free}) must not exceed total ({total})"
        );
        assert_ne!(
            (free, total),
            (u64::MAX, u64::MAX),
            "statvfs(/tmp) should succeed"
        );
    }

    #[test]
    fn missing_path_falls_back_to_sentinel() {
        let f = disk_free_for(Path::new("/nonexistent/path/that/should/never/exist/42"));
        let (free, total) = f();
        assert_eq!((free, total), (u64::MAX, u64::MAX));
    }
}
