//! Release-state classifier (ADR-0050 § 1).
//!
//! Audible's catalog API returns a `release_date` /
//! `publication_datetime` field on every product. Past dates are
//! straightforward (released titles); future dates are scheduled
//! releases; **the literal string `2200-01-01T00:00:00Z` is a
//! sentinel** indicating an announced-but-no-date title — Audible
//! has accepted the listing but hasn't committed to a release
//! window yet.
//!
//! Libex (MIT, see ADR-0050) filters sentinel-dated rows out of
//! its catalog responses. We do the opposite — keep them and tag
//! them as `Announced`, because that's exactly the signal the
//! `/upcoming` surface needs for "books followed authors have
//! pre-announced."
//!
//! This module ships the classifier only. The `/upcoming`
//! endpoint, the author-bibliography fetch path (ADR-0050 § 2),
//! and the schedule-sorted display will land as their own slices
//! once the consumer surface is built.

use chrono::{DateTime, Utc};

/// Audible's sentinel for announced-but-no-date listings.
///
/// Discovered via Libex's filter constant
/// (`UNRELEASED_PLACEHOLDER` in `app/services/audible/books.py`).
/// The string is matched literally rather than parsed-then-
/// compared so a typo in either direction trips an explicit
/// match failure instead of silently coincidentally treating
/// a real future date as the sentinel.
pub const AUDIBLE_UNRELEASED_SENTINEL: &str = "2200-01-01T00:00:00Z";

/// Three buckets a catalog product can fall into per its
/// `release_date` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseState {
    /// Already published — `release_date` is in the past
    /// relative to "now".
    Released,
    /// Future-dated with a real date. Pre-orderable.
    Scheduled,
    /// Audible holds the listing but no release window —
    /// `release_date` is the literal
    /// [`AUDIBLE_UNRELEASED_SENTINEL`] string.
    Announced,
    /// Field missing, unparseable, or otherwise unusable.
    /// Caller decides whether to treat as "released" (likely
    /// pre-existing back-catalog), "skipped" (don't show in
    /// /upcoming), or "needs investigation."
    Unknown,
}

/// Classify a raw `release_date` string from Audible JSON
/// against `now`.
///
/// `release_date` accepts the formats Audible actually emits:
///
/// * RFC 3339 / ISO 8601 with `Z` (`2025-04-15T00:00:00Z`)
/// * RFC 3339 with offset (`2025-04-15T00:00:00-07:00`)
/// * Date-only (`2025-04-15`) — common on older listings
///
/// The sentinel match is **literal** — testing the input string
/// against [`AUDIBLE_UNRELEASED_SENTINEL`] before parsing
/// shortcuts on the sentinel even if some future Audible change
/// makes the year-2200 timestamp parse-but-different (timezone
/// offset variant, etc.).
#[must_use]
pub fn classify(release_date: Option<&str>, now: DateTime<Utc>) -> ReleaseState {
    let Some(s) = release_date.map(str::trim).filter(|s| !s.is_empty()) else {
        return ReleaseState::Unknown;
    };

    if s == AUDIBLE_UNRELEASED_SENTINEL {
        return ReleaseState::Announced;
    }

    let parsed = parse_release_datetime(s);
    match parsed {
        Some(dt) if dt > now => ReleaseState::Scheduled,
        Some(_) => ReleaseState::Released,
        None => ReleaseState::Unknown,
    }
}

/// Parse the three formats Audible actually emits. Returns
/// `None` on any failure (caller routes to `Unknown`).
fn parse_release_datetime(s: &str) -> Option<DateTime<Utc>> {
    // Full RFC 3339 — handles `Z`, explicit offsets, fractional
    // seconds, etc. in one go.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // Date-only — older listings. Treat as midnight UTC.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|n| n.and_utc());
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("test date parses")
            .with_timezone(&Utc)
    }

    #[test]
    fn sentinel_string_classifies_as_announced() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(
            classify(Some(AUDIBLE_UNRELEASED_SENTINEL), now),
            ReleaseState::Announced
        );
    }

    #[test]
    fn past_rfc3339_date_is_released() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(
            classify(Some("2020-03-01T00:00:00Z"), now),
            ReleaseState::Released
        );
    }

    #[test]
    fn future_rfc3339_date_is_scheduled() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(
            classify(Some("2027-01-15T00:00:00Z"), now),
            ReleaseState::Scheduled
        );
    }

    #[test]
    fn date_only_past_is_released() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(classify(Some("2020-03-01"), now), ReleaseState::Released);
    }

    #[test]
    fn date_only_future_is_scheduled() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(classify(Some("2027-01-15"), now), ReleaseState::Scheduled);
    }

    #[test]
    fn rfc3339_with_offset_handled() {
        let now = at("2026-05-15T00:00:00Z");
        // 2025-12-31T18:00:00-08:00 = 2026-01-01T02:00:00Z → past
        assert_eq!(
            classify(Some("2025-12-31T18:00:00-08:00"), now),
            ReleaseState::Released
        );
    }

    #[test]
    fn none_input_is_unknown() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(classify(None, now), ReleaseState::Unknown);
    }

    #[test]
    fn empty_string_is_unknown() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(classify(Some(""), now), ReleaseState::Unknown);
    }

    #[test]
    fn whitespace_only_string_is_unknown() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(classify(Some("   "), now), ReleaseState::Unknown);
    }

    #[test]
    fn unparseable_string_is_unknown() {
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(classify(Some("not-a-date"), now), ReleaseState::Unknown);
    }

    #[test]
    fn release_date_equal_to_now_classifies_as_released() {
        let now = at("2026-05-15T12:00:00Z");
        // `dt > now` is strict-greater; equal counts as released
        // (the listing is live, even if only just).
        assert_eq!(
            classify(Some("2026-05-15T12:00:00Z"), now),
            ReleaseState::Released
        );
    }

    #[test]
    fn sentinel_match_is_literal_not_parsed() {
        // If we parsed first then checked "is this year 2200?",
        // a variant like `2200-01-01` (date-only) might
        // accidentally be classified as Announced. The literal
        // string match prevents that — date-only year-2200 is
        // a regular Scheduled (very-far-future) date.
        let now = at("2026-05-15T00:00:00Z");
        assert_eq!(classify(Some("2200-01-01"), now), ReleaseState::Scheduled);
    }

    #[test]
    fn sentinel_const_matches_libex_constant() {
        // Regression guard: this string MUST stay in sync with
        // Libex's UNRELEASED_PLACEHOLDER and any future Audible
        // change. A change here without a corresponding change
        // to the schema migration / catalog adapter is a bug.
        assert_eq!(AUDIBLE_UNRELEASED_SENTINEL, "2200-01-01T00:00:00Z");
    }
}
