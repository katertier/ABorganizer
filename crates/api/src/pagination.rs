//! Shared pagination helpers for the entity-list endpoints
//! (`/authors`, `/narrators`, `/series`).
//!
//! The three list endpoints all accept the same `?limit=&offset=`
//! query-string params with the same clamping semantics; this
//! module is the one place those rules live. Sort enums + the
//! per-entity SQL stay in the entity-specific modules — only the
//! generic pagination knobs are shared here.
//!
//! ## Why constants live here, not in [`ab_core::tunables`]
//!
//! These limits are API-shape decisions (clients see `limit=200`
//! as the max), not tunables the operator changes per
//! deployment. Promoting them to `Tunables` would invite per-
//! environment drift in JSON-response shapes, which would be
//! visible to every client. Keep them code-level + workspace-
//! wide constant.

/// Default `limit` when callers omit the query param. Picked to
/// keep the most common "browse the catalogue" call cheap while
/// still being one screen of results on a typical UI.
pub const DEFAULT_LIMIT: i64 = 50;

/// Hard ceiling on `limit`. Larger requests clamp silently
/// (no `400 Bad Request`) — same posture as `books_list`.
///
/// Picked to bound the worst-case response size; a 100k-book
/// library with all-author pages of 200 rows each still fits
/// comfortably in the typical browser memory budget for one
/// fetch.
pub const MAX_LIMIT: i64 = 200;

/// Clamp `limit` to `[1, MAX_LIMIT]` with [`DEFAULT_LIMIT`] as
/// the fallback for absent / non-positive values.
///
/// * `None` → [`DEFAULT_LIMIT`]
/// * `Some(n)` where `n <= 0` → `1`
/// * `Some(n)` where `n > MAX_LIMIT` → [`MAX_LIMIT`]
/// * Otherwise → `n`
#[must_use]
pub const fn clamp_limit(raw: Option<i64>) -> i64 {
    match raw {
        None => DEFAULT_LIMIT,
        Some(n) if n <= 0 => 1,
        Some(n) if n > MAX_LIMIT => MAX_LIMIT,
        Some(n) => n,
    }
}

/// Clamp `offset` to non-negative. Absent → `0`.
#[must_use]
pub const fn clamp_offset(raw: Option<i64>) -> i64 {
    match raw {
        None => 0,
        Some(n) if n < 0 => 0,
        Some(n) => n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_respects_bounds() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
        assert_eq!(clamp_limit(Some(MAX_LIMIT)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(MAX_LIMIT + 100)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(75)), 75);
        // i64::MAX should clamp without overflow.
        assert_eq!(clamp_limit(Some(i64::MAX)), MAX_LIMIT);
    }

    #[test]
    fn clamp_offset_respects_bounds() {
        assert_eq!(clamp_offset(None), 0);
        assert_eq!(clamp_offset(Some(-1)), 0);
        assert_eq!(clamp_offset(Some(i64::MIN)), 0);
        assert_eq!(clamp_offset(Some(0)), 0);
        assert_eq!(clamp_offset(Some(250)), 250);
    }
}
