//! Cleanup subsystem primitives (slice H.2, ADR-0025).
//!
//! Pure types used by every cleanup target: the [`Category`]
//! taxonomy, the [`Policy`] passed to each tick, the
//! [`CleanupReport`] wire shape, and the pressure-ratchet
//! arithmetic [`compute_age_seconds`].
//!
//! The [`crate::CleanupTarget`] trait itself + the periodic
//! loop live in `ab_pipeline::cleanup` so they can reference
//! `LibraryDb` / `EphemeralDb` (which depend on `ab_core`,
//! the other direction of the dep arrow). Per-crate targets
//! `impl ab_pipeline::CleanupTarget for ...` in their owning
//! crate.

use serde::{Deserialize, Serialize};

/// Top-level category every cleanup target self-identifies as.
/// Drives the `aborg clean disk|db|queue` CLI routing + the
/// per-category usage reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Filesystem artifacts (transcript frames, transcoding
    /// temps, cover caches).
    Disk,
    /// Library + ephemeral row pruning that isn't on the
    /// pipeline's correctness path. Cross-DB targets allowed.
    Db,
    /// Auth + rate-limit + job-queue table pruning.
    Queue,
}

impl Category {
    /// Lowercase canonical string. Matches the JSON wire format.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Db => "db",
            Self::Queue => "queue",
        }
    }

    /// Parse from the lowercase canonical string. Returns
    /// `None` on an unknown name so the caller can surface a
    /// `400 Bad Request` with the valid set.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "disk" => Some(Self::Disk),
            "db" => Some(Self::Db),
            "queue" => Some(Self::Queue),
            _ => None,
        }
    }

    /// Every variant in display order. Used by the API's
    /// 400-response body to surface the valid set, and by
    /// the CLI's no-category-given summary report.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Disk, Self::Db, Self::Queue]
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-tick policy passed to every target.
///
/// `age_seconds` is the cut-off computed by the disk-pressure
/// ratchet; targets delete items older than this. `force`
/// bypasses the age gate (operator opt-in); `apply`
/// distinguishes dry-run from real cleanup.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Age threshold in seconds — items older are eligible.
    /// Set by [`compute_age_seconds`]; bypassed when `force`.
    pub age_seconds: i64,
    /// `true` → ignore `age_seconds`, treat every item as
    /// eligible. Set by `--force` on the manual surface.
    pub force: bool,
    /// `false` → dry-run; the handler asks targets to
    /// `report` but never to `apply`.
    pub apply: bool,
}

impl Policy {
    /// Cheap constructor for a baseline-age dry-run.
    #[must_use]
    pub const fn dry_run(age_seconds: i64) -> Self {
        Self {
            age_seconds,
            force: false,
            apply: false,
        }
    }
}

/// What a target would do (dry-run) or did do (apply).
///
/// The two paths share a struct so the API response is
/// uniform — `items` + `bytes` mean the same thing in both
/// modes.
#[derive(Debug, Clone, Serialize)]
pub struct CleanupReport {
    /// Category the target self-identifies as.
    pub category: Category,
    /// Stable per-target name. Used in CLI output + logs.
    pub name: String,
    /// Count of items the target would prune (or pruned).
    pub items: u64,
    /// Bytes the target would free (or freed). Targets that
    /// can't cheaply estimate size return 0 — the count is
    /// the operative signal.
    pub bytes: u64,
}

impl CleanupReport {
    /// Cheap empty report — "nothing eligible right now."
    #[must_use]
    pub fn empty(category: Category, name: &'static str) -> Self {
        Self {
            category,
            name: name.to_owned(),
            items: 0,
            bytes: 0,
        }
    }
}

/// Pick the right `age_seconds` for the current disk-pressure level.
///
/// Walks [`crate::tunables::CleanupTunables::pressure`] tiers
/// in order; first tier whose `free_percent` or `free_bytes`
/// threshold is reached wins.
///
/// `free_bytes` / `total_bytes` come from the platform's
/// `statvfs` (or equivalent). On platforms where that lookup
/// fails the caller passes `(u64::MAX, u64::MAX)` — every
/// pressure tier remains untriggered and the baseline age
/// applies.
///
/// All tiers are evaluated; the smallest matching `age_days`
/// wins (most-aggressive cleanup). Floor of 1 day so a zero-
/// `default_age_days` tunable can't accidentally delete fresh
/// state.
#[must_use]
pub fn compute_age_seconds(
    tunables: &crate::tunables::CleanupTunables,
    free_bytes: u64,
    total_bytes: u64,
) -> i64 {
    let mut age_days = tunables.default_age_days;
    for tier in &tunables.pressure {
        let percent_hit = tier
            .free_percent
            .is_some_and(|p| triggers_percent(free_bytes, total_bytes, p));
        let bytes_hit = tier.free_bytes.is_some_and(|b| free_bytes < b);
        if percent_hit || bytes_hit {
            age_days = age_days.min(tier.age_days);
        }
    }
    let age_days_clamped = age_days.max(1);
    i64::try_from(age_days_clamped.saturating_mul(86_400)).unwrap_or(i64::MAX)
}

/// True iff free space is below `pct` (as a percentage of
/// total). Saturates on the empty-disk edge case
/// (`total == 0` → "always under pressure," which is the
/// conservative read).
fn triggers_percent(free_bytes: u64, total_bytes: u64, pct: f32) -> bool {
    if total_bytes == 0 {
        return true;
    }
    #[allow(clippy::cast_precision_loss)]
    let free = free_bytes as f64;
    #[allow(clippy::cast_precision_loss)]
    let total = total_bytes as f64;
    let fraction = free / total * 100.0;
    fraction < f64::from(pct)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tunables::{CleanupTunables, PressureTier};

    #[test]
    fn category_parses_lowercase() {
        assert_eq!(Category::parse("disk"), Some(Category::Disk));
        assert_eq!(Category::parse("db"), Some(Category::Db));
        assert_eq!(Category::parse("queue"), Some(Category::Queue));
        assert_eq!(Category::parse("DISK"), None);
        assert_eq!(Category::parse("trash"), None);
    }

    #[test]
    fn baseline_age_used_when_no_pressure_tier_triggers() {
        let t = CleanupTunables {
            check_secs: 3_600,
            default_age_days: 14,
            pressure: vec![PressureTier {
                free_percent: Some(10.0),
                free_bytes: None,
                age_days: 7,
            }],
        };
        let age = compute_age_seconds(&t, 500, 1_000);
        assert_eq!(age, 14 * 86_400);
    }

    #[test]
    fn pressure_tier_picks_smaller_age() {
        let t = CleanupTunables {
            check_secs: 3_600,
            default_age_days: 14,
            pressure: vec![
                PressureTier {
                    free_percent: Some(10.0),
                    free_bytes: None,
                    age_days: 7,
                },
                PressureTier {
                    free_percent: Some(5.0),
                    free_bytes: None,
                    age_days: 3,
                },
            ],
        };
        // 4% free → both tiers hit (4 < 10, 4 < 5); minimum (3) wins.
        let age = compute_age_seconds(&t, 40, 1_000);
        assert_eq!(age, 3 * 86_400);
        // 7% free → only the 10% tier hits (7 < 10 but 7 > 5).
        let age = compute_age_seconds(&t, 70, 1_000);
        assert_eq!(age, 7 * 86_400);
        // 12% free → no tier hits; baseline applies.
        let age = compute_age_seconds(&t, 120, 1_000);
        assert_eq!(age, 14 * 86_400);
    }

    #[test]
    fn absolute_bytes_threshold_triggers_independently() {
        let t = CleanupTunables {
            check_secs: 3_600,
            default_age_days: 14,
            pressure: vec![PressureTier {
                free_percent: None,
                free_bytes: Some(1_000),
                age_days: 3,
            }],
        };
        let age = compute_age_seconds(&t, 500, 1_000);
        assert_eq!(age, 3 * 86_400);
        let age = compute_age_seconds(&t, 5_000, 10_000);
        assert_eq!(age, 14 * 86_400);
    }

    #[test]
    fn floor_of_one_day_protects_against_zero_tunable() {
        let t = CleanupTunables {
            check_secs: 3_600,
            default_age_days: 0,
            pressure: vec![],
        };
        let age = compute_age_seconds(&t, u64::MAX, u64::MAX);
        assert_eq!(age, 86_400);
    }
}
