//! Series gap detection (ADR-0050 § 4).
//!
//! Audible's `/1.0/catalog/products/{series_asin}?response_groups=relationships`
//! returns a list of book references with a `sort` field — a
//! floating-point position used to order the series. Integer
//! values are main entries (Book 1, 2, 3); non-integer values
//! are novellas / 0.5 / 1.5 entries that interleave with the
//! main run.
//!
//! Gap detection is pure arithmetic over these floats:
//!
//! * **Integer gaps** — for the range `[min_int .. max_int]` of
//!   present integer-valued positions, any integer not represented
//!   is a gap (a book the operator is missing if they think they
//!   own the series).
//! * **Fractional entries** — positions whose value isn't an integer
//!   (within the [`F64_EPSILON`] tolerance for IEEE 754 round-trip
//!   safety). Surfacing these helps the operator notice novellas
//!   that they may not have heard of when buying the main run.
//!
//! ## Design choices
//!
//! * Input is `&[f64]`, not a richer `SeriesEntry` struct. The
//!   ASIN / title columns aren't load-bearing for the math; the
//!   caller already has them and can render gaps however the
//!   surface needs. Keeping the function shape minimal means the
//!   tests are pure-data + the same helper can serve any future
//!   consumer (CLI, web GUI, podcast feed playlist generator).
//! * Integer detection uses [`F64_EPSILON`] = 1e-6 (well below
//!   the granularity any sane series-sort field would ever use).
//!   Audible's own values are typically exact integers or `.5`
//!   halves; floats only enter the picture because the JSON
//!   parser decodes the column that way.
//! * The "max" cap is inclusive: gaps `[3, 5]` against positions
//!   `[1, 2, 4, 5]` produce `[3]`, not `[3, 5]`. We only flag
//!   missing entries *inside* the observed range, never beyond
//!   it (a series that ends at book 5 in Audible's catalog is
//!   complete at 5 — flagging "6" as missing would be wrong).
//! * Empty input → empty result, no error path. Single-entry
//!   input → no gaps. The smallest interesting case is two
//!   entries with a gap between them.

/// Tolerance below which an `f64` is treated as an integer.
/// 1e-6 is well below any reasonable series-sort granularity and
/// above IEEE 754 round-trip noise on `Vec<f64>` deserialisation.
const F64_EPSILON: f64 = 1e-6;

/// Integer-valued gaps within the observed `[min_int .. max_int]`
/// range of `positions`.
///
/// Returns the missing integers sorted ascending. Positions
/// outside the range (a series ending at 5 doesn't flag 6 as
/// missing) are never included.
///
/// Examples:
/// * `[1.0, 2.0, 3.0]` → `[]` (no gaps)
/// * `[1.0, 3.0, 5.0]` → `[2, 4]`
/// * `[1.0, 1.5, 2.0, 4.0]` → `[3]` (fractional 1.5 ignored)
/// * `[]` or `[1.0]` → `[]` (no range)
#[must_use]
pub fn integer_gaps(positions: &[f64]) -> Vec<u32> {
    let mut integers: Vec<u32> = positions
        .iter()
        .filter_map(|&p| to_u32_if_integer(p))
        .collect();
    if integers.len() < 2 {
        return Vec::new();
    }
    integers.sort_unstable();
    integers.dedup();

    let min = integers[0];
    let max = integers[integers.len() - 1];
    let mut out = Vec::new();
    for n in min..=max {
        if integers.binary_search(&n).is_err() {
            out.push(n);
        }
    }
    out
}

/// Convert an f64 position to a u32 iff it's near-integer,
/// non-negative, and within u32's range. Returns None otherwise.
/// Replaces a previous `as i64` cast that tripped
/// `clippy::cast_possible_truncation`.
fn to_u32_if_integer(v: f64) -> Option<u32> {
    if !is_integer(v) || v < 0.0 || v > f64::from(u32::MAX) {
        return None;
    }
    // Safe: bounded above by u32::MAX, integer-valued within
    // F64_EPSILON, non-negative. `f64::to_bits` round-trips
    // exactly for integer values up to 2^53, well above u32::MAX.
    let rounded = v.round();
    // `as u32` is well-defined for non-negative finite f64 in the
    // valid range; the prior checks make this lossless. Clippy
    // is still suspicious of float→int casts (any of them); a
    // targeted allow at this single call site documents intent.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(rounded as u32)
}

/// Positions that are not integers (novellas, 0.5 / 1.5 / 2.5
/// entries). Returned in input order, dedup'd, and stable so the
/// caller can render them next to titles deterministically.
#[must_use]
pub fn fractional_positions(positions: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = positions
        .iter()
        .copied()
        .filter(|&p| !is_integer(p))
        .collect();
    // Dedup preserving order: simple O(n²) is fine — series rarely
    // have more than a dozen entries, never mind hundreds.
    let mut seen: Vec<f64> = Vec::new();
    out.retain(|&v| {
        let already = seen.iter().any(|&s| (s - v).abs() < F64_EPSILON);
        if !already {
            seen.push(v);
        }
        !already
    });
    out
}

/// Combined result for a single series — both kinds of gap.
///
/// Returned as a small struct so callers can render the two
/// findings together without re-walking the position list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeriesGaps {
    /// Integer gaps within `[min..max]` of the observed positions.
    pub integer_gaps: Vec<u32>,
    /// Fractional entries (typically novellas / `N.5` books).
    pub fractional_entries: Vec<f64>,
}

/// Compute both views in one call.
#[must_use]
pub fn compute(positions: &[f64]) -> SeriesGaps {
    SeriesGaps {
        integer_gaps: integer_gaps(positions),
        fractional_entries: fractional_positions(positions),
    }
}

fn is_integer(v: f64) -> bool {
    (v - v.round()).abs() < F64_EPSILON
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_gaps() {
        let g = compute(&[]);
        assert!(g.integer_gaps.is_empty());
        assert!(g.fractional_entries.is_empty());
    }

    #[test]
    fn single_entry_has_no_gaps() {
        let g = compute(&[3.0]);
        assert!(g.integer_gaps.is_empty());
        assert!(g.fractional_entries.is_empty());
    }

    #[test]
    fn contiguous_run_has_no_integer_gaps() {
        let g = integer_gaps(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!(g.is_empty());
    }

    #[test]
    fn detects_single_integer_gap() {
        let g = integer_gaps(&[1.0, 2.0, 4.0, 5.0]);
        assert_eq!(g, vec![3]);
    }

    #[test]
    fn detects_multiple_integer_gaps() {
        let g = integer_gaps(&[1.0, 3.0, 5.0]);
        assert_eq!(g, vec![2, 4]);
    }

    #[test]
    fn does_not_flag_beyond_observed_range() {
        // Series ends at 5 — book 6 should NOT be flagged.
        let g = integer_gaps(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!(g.is_empty(), "got {g:?}");
    }

    #[test]
    fn fractional_position_does_not_create_integer_gap() {
        // 1.5 is a novella between 1 and 2 — it doesn't claim
        // the integer slot "2".
        let g = integer_gaps(&[1.0, 1.5, 2.0, 3.0]);
        assert!(g.is_empty(), "got {g:?}");
    }

    #[test]
    fn fractional_entries_surfaced() {
        let g = fractional_positions(&[1.0, 1.5, 2.0, 2.5, 3.0]);
        assert_eq!(g, vec![1.5, 2.5]);
    }

    #[test]
    fn fractional_dedup_preserves_first_occurrence() {
        let g = fractional_positions(&[1.5, 2.0, 1.5, 2.5]);
        assert_eq!(g, vec![1.5, 2.5]);
    }

    #[test]
    fn out_of_order_input_still_works() {
        // Audible's response order isn't guaranteed; we sort.
        let g = integer_gaps(&[5.0, 1.0, 3.0]);
        assert_eq!(g, vec![2, 4]);
    }

    #[test]
    fn duplicate_integer_positions_collapse() {
        // Two rows of "Book 2" (data error / multi-edition) →
        // one slot consumed, gaps computed correctly.
        let g = integer_gaps(&[1.0, 2.0, 2.0, 4.0]);
        assert_eq!(g, vec![3]);
    }

    #[test]
    fn combined_result_returns_both_views() {
        let g = compute(&[1.0, 1.5, 3.0]);
        assert_eq!(g.integer_gaps, vec![2]);
        assert_eq!(g.fractional_entries, vec![1.5]);
    }

    #[test]
    fn near_integer_within_epsilon_treated_as_integer() {
        // IEEE 754 round-trip noise on JSON `1.0` may decode as
        // 0.9999999999 or 1.0000000001 in degenerate parsers; the
        // epsilon tolerance keeps gap detection robust against
        // such inputs.
        let near = 2.0 + (F64_EPSILON / 10.0);
        let g = integer_gaps(&[1.0, near, 3.0]);
        assert!(
            g.is_empty(),
            "near-integer {near} should fill the 2-slot, got {g:?}"
        );
    }

    #[test]
    fn negative_positions_skipped() {
        // Defensive: Audible should never emit negative sort
        // values, but if it did we shouldn't blow up the u32
        // conversion.
        let g = integer_gaps(&[-1.0, 1.0, 3.0]);
        assert_eq!(g, vec![2]);
    }
}
