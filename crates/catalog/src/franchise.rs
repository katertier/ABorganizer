//! Franchise prefix detection (slice 10B, Cluster 1).
//!
//! Series like "Star Wars" produce books titled
//! "Star Wars: A New Hope" / "Star Wars: Sarek" — the franchise
//! name precedes a delimiter, then the per-book title. For
//! correct sort order and search behaviour we want:
//!
//! - `series.franchise_prefix` = "Star Wars: " (auto-detected from
//!   the LCP across the series' member titles).
//! - `books.title_sort` = "A New Hope" / "Sarek" (the franchise
//!   stripped). Sort by `title_sort`, display by `title`, search
//!   strips the franchise on miss.
//!
//! Triggered from `identity-resolve` after series resolution lands
//! the `book_series` rows for a book. Runs once per series the
//! book belongs to; computes the LCP from the series' member
//! titles and either updates `franchise_prefix` (and the affected
//! books' `title_sort`) or leaves both alone if no clean prefix
//! emerges.
//!
//! Conservative — a "franchise" must:
//!
//! 1. Be at least 4 characters long (one-word prefixes are noise).
//! 2. End in a recognised delimiter (`: `, ` - `, ` — `).
//! 3. Appear before the delimiter in ≥ 2 member titles.
//!
//! These rules keep "The " / "A " from accidentally becoming
//! franchises.

use ab_core::{Error, Result};

/// Recognised franchise delimiters. The detector keeps the
/// delimiter in the stored prefix so `title.starts_with(prefix)` is
/// the only check `title_sort` needs.
const FRANCHISE_DELIMITERS: &[&str] = &[": ", " - ", " \u{2014} "];

/// Minimum length for a franchise prefix (delimiter included).
/// "A: " or "I- " would otherwise qualify and produce nonsense.
const MIN_FRANCHISE_LEN: usize = 4;

/// Recompute `series.franchise_prefix` for every series the given
/// `book_id` belongs to + update `books.title_sort` for every
/// member book.
///
/// Returns the count of `series.franchise_prefix` rows changed
/// (NULL → value, value → different value, or value → NULL).
pub async fn recompute_franchise_for_book(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    book_id: i64,
) -> Result<usize> {
    let series_rows = sqlx::query!(
        "SELECT bs.series_id AS \"series_id!: i64\", \
                s.franchise_prefix \
         FROM book_series bs \
         JOIN series s ON s.series_id = bs.series_id \
         WHERE bs.book_id = ?",
        book_id,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("franchise: read book's series: {e}")))?;

    let mut changed = 0;
    for series in series_rows {
        let new_prefix = detect_for_series(tx, series.series_id).await?;
        if new_prefix.as_deref() != series.franchise_prefix.as_deref() {
            sqlx::query!(
                "UPDATE series SET franchise_prefix = ? WHERE series_id = ?",
                new_prefix,
                series.series_id,
            )
            .execute(&mut **tx)
            .await
            .map_err(|e| Error::Database(format!("franchise: update series: {e}")))?;
            changed += 1;
        }
        recompute_title_sort_for_series(tx, series.series_id, new_prefix.as_deref()).await?;
    }
    Ok(changed)
}

/// Compute the franchise prefix for one series. Reads member-book
/// titles, returns the detected prefix or `None` if no clean one
/// emerges.
async fn detect_for_series(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    series_id: i64,
) -> Result<Option<String>> {
    let titles: Vec<String> = sqlx::query_scalar!(
        "SELECT b.title FROM books b \
         JOIN book_series bs ON bs.book_id = b.book_id \
         WHERE bs.series_id = ?",
        series_id,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("franchise: read series titles: {e}")))?;

    if titles.len() < 2 {
        return Ok(None);
    }
    let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
    Ok(detect_prefix(&refs))
}

/// Pure detector: longest common prefix that ends in a recognised
/// delimiter and clears [`MIN_FRANCHISE_LEN`]. `None` if no clean
/// prefix qualifies.
#[must_use]
pub fn detect_prefix(titles: &[&str]) -> Option<String> {
    if titles.len() < 2 {
        return None;
    }
    let lcp = longest_common_prefix(titles);
    if lcp.len() < MIN_FRANCHISE_LEN {
        return None;
    }
    // Trim back to the last delimiter ending so the stored prefix
    // ends cleanly ("Star Wars: " rather than "Star Wars: A").
    for delim in FRANCHISE_DELIMITERS {
        if let Some(idx) = lcp.rfind(delim) {
            let cut = idx + delim.len();
            if cut >= MIN_FRANCHISE_LEN {
                return Some(lcp[..cut].to_owned());
            }
        }
    }
    None
}

/// Longest common prefix of a non-empty slice of strings. Returns
/// an empty string when no common prefix exists.
fn longest_common_prefix(strs: &[&str]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    let mut prefix_end = strs[0].len();
    let first = strs[0];
    for s in &strs[1..] {
        let mut i = 0;
        let mut iter_a = first.char_indices();
        let mut iter_b = s.char_indices();
        loop {
            match (iter_a.next(), iter_b.next()) {
                (Some((idx, ca)), Some((_, cb))) if ca == cb => {
                    i = idx + ca.len_utf8();
                }
                _ => break,
            }
        }
        prefix_end = prefix_end.min(i);
        if prefix_end == 0 {
            return String::new();
        }
    }
    first[..prefix_end].to_owned()
}

/// Recompute `books.title_sort` for every member book of a series
/// after its `franchise_prefix` changed. Strip the prefix from
/// `title` when `title` starts with it; clear `title_sort`
/// otherwise.
async fn recompute_title_sort_for_series(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    series_id: i64,
    prefix: Option<&str>,
) -> Result<()> {
    let books = sqlx::query!(
        "SELECT b.book_id AS \"book_id!: i64\", b.title \
         FROM books b JOIN book_series bs ON bs.book_id = b.book_id \
         WHERE bs.series_id = ?",
        series_id,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| Error::Database(format!("franchise: read series books: {e}")))?;
    for b in books {
        let new_sort = match prefix {
            Some(p) if b.title.starts_with(p) => Some(b.title[p.len()..].trim_start().to_owned()),
            _ => None,
        };
        sqlx::query!(
            "UPDATE books SET title_sort = ? WHERE book_id = ?",
            new_sort,
            b.book_id,
        )
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::Database(format!("franchise: update title_sort: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn lcp_basic() {
        assert_eq!(longest_common_prefix(&["abc", "abd"]), "ab");
        assert_eq!(longest_common_prefix(&["abc", "xyz"]), "");
        assert_eq!(longest_common_prefix(&["same", "same"]), "same");
    }

    #[test]
    fn detect_star_wars_franchise() {
        let titles = [
            "Star Wars: A New Hope",
            "Star Wars: The Empire Strikes Back",
            "Star Wars: Sarek",
        ];
        assert_eq!(detect_prefix(&titles), Some("Star Wars: ".to_owned()));
    }

    #[test]
    fn detect_with_dash_delimiter() {
        let titles = ["Foundation - Empire", "Foundation - Second Foundation"];
        assert_eq!(detect_prefix(&titles), Some("Foundation - ".to_owned()));
    }

    #[test]
    fn single_title_returns_none() {
        assert_eq!(detect_prefix(&["Only Book: Subtitle"]), None);
    }

    #[test]
    fn no_common_delimiter_returns_none() {
        let titles = ["The Way of Kings", "Words of Radiance"];
        // LCP empty → no franchise. (Sanderson titles share no prefix.)
        assert_eq!(detect_prefix(&titles), None);
    }

    #[test]
    fn short_prefix_below_min_len_rejected() {
        let titles = ["A: One", "A: Two"];
        // "A: " is only 3 chars — below MIN_FRANCHISE_LEN of 4.
        assert_eq!(detect_prefix(&titles), None);
    }

    #[test]
    fn prefix_without_delimiter_rejected() {
        // LCP is "The " but neither delimiter follows the space.
        let titles = ["The Final Empire", "The Way of Kings"];
        // The LCP "The " doesn't end in `:`, `- `, etc., so reject.
        assert_eq!(detect_prefix(&titles), None);
    }
}
