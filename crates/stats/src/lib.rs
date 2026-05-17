//! Library stats — counts, listening totals, pie-chart breakdowns
//! (ADR-0044, slice B.17).
//!
//! Two surfaces:
//!
//! * [`stats`] — counts + listening totals (single-row response).
//! * [`breakdown`] — pie-chart data for a [`Dimension`].
//!
//! All queries run against `library.db`. Stats are recomputed on
//! every request — no cache. At v1.0 sizes (20–100k books) the
//! aggregations stay under 100ms on local SQLite.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // scaffold; tightened in follow-up slices

use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("unsupported dimension: {0}")]
    UnsupportedDimension(String),
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StatsResponse {
    pub counts: Counts,
    pub listening: Listening,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counts {
    pub total: u64,
    pub finished_year: u64,
    pub finished_all_time: u64,
    pub unread: u64,
    pub reading: u64,
    pub dnf: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Listening {
    pub hours_year: f64,
    pub hours_all_time: f64,
    pub longest_streak_days: u32,
    pub avg_book_hours: f64,
    pub avg_daily_minutes: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BreakdownResponse {
    pub dimension: String,
    pub buckets: Vec<Bucket>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bucket {
    pub label: String,
    pub count: u64,
    pub hours: f64,
    pub percentage: f32,
}

/// Pie-chart dimensions supported in this slice. `Loudness`
/// arrives once B.20 ships `books.lufs_integrated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Language,
    Length,
    ReadingStatus,
    AcquisitionYear,
    /// Release-date decade (`2010s`, `1990s`, …). Bucketed from
    /// `books.release_date` (ISO 8601 date string from Audnexus /
    /// tag-read); books without a parseable `release_date` roll
    /// into `unknown`.
    Decade,
    /// Books per publisher. Joins through `publishers.name` via
    /// `books.publisher_id`; books with no publisher fall into
    /// the `unknown` bucket. The `TOP_N` collapse-long-tail step
    /// keeps the response bounded on libraries with many
    /// small-publisher entries.
    Publisher,
    /// Books per audio container format (`m4b`, `mp3`, `flac`, …).
    /// Takes the lowercased `format` of each book's first active
    /// file (`MIN(file_id) WHERE is_active = 1`); books with no
    /// active file or no recorded format roll into `unknown`.
    Format,
    /// Books per author. Joins through `authors.name` via
    /// `books.author_id` (single-FK — multi-author books are not
    /// modelled today, so every book lands in exactly one bucket
    /// or `unknown`). The `TOP_N` collapse-long-tail step keeps
    /// the response bounded; operator libraries with many
    /// one-off authors naturally roll a heavy tail into `Others`.
    Author,
    /// Books per narrator. Joins through the `book_narrator`
    /// bridge — multi-narrator (full-cast) recordings contribute
    /// one row per (book, narrator) pair, so a 3-cast book lands
    /// in 3 narrator buckets. The bucket count is therefore the
    /// number of books a narrator participated in (not the total
    /// number of distinct books across all buckets); `hours_ms`
    /// likewise over-sums for full-cast recordings. Books with no
    /// `book_narrator` row roll into `unknown`.
    Narrator,
    /// Books per series. Joins through the `book_series` bridge —
    /// a book that belongs to multiple series (rare but allowed
    /// for spin-offs / crossovers) contributes one row per
    /// (book, series) pair, so it lands in multiple buckets.
    /// `bucket.count` is the number of books a series catalogs
    /// (primary + secondary memberships); `hours_ms` similarly
    /// over-sums when multi-series books exist. Books with no
    /// `book_series` row roll into `unknown` (standalones).
    Series,
    /// Books per `books.audiologo_status`. Values come from the
    /// schema CHECK constraint (migration 010): `unknown`,
    /// `detected`, `applied`, `stripped`, `none`, `rejected`. No
    /// JOIN — pure column groupby on a small enum-of-strings.
    /// Useful for "how much of the library still needs an
    /// audiologo trim pass?" — `applied` + `none` + `rejected`
    /// are settled; `detected` is the operator review queue;
    /// `unknown` hasn't been touched yet.
    AudiologoStatus,
}

impl Dimension {
    pub fn parse(s: &str) -> Result<Self, StatsError> {
        Ok(match s {
            "language" => Self::Language,
            "length" => Self::Length,
            "reading_status" => Self::ReadingStatus,
            "acquisition_year" => Self::AcquisitionYear,
            "decade" => Self::Decade,
            "publisher" => Self::Publisher,
            "format" => Self::Format,
            "author" => Self::Author,
            "narrator" => Self::Narrator,
            "series" => Self::Series,
            "audiologo_status" => Self::AudiologoStatus,
            other => return Err(StatsError::UnsupportedDimension(other.to_owned())),
        })
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Language => "language",
            Self::Length => "length",
            Self::ReadingStatus => "reading_status",
            Self::AcquisitionYear => "acquisition_year",
            Self::Decade => "decade",
            Self::Publisher => "publisher",
            Self::Format => "format",
            Self::Author => "author",
            Self::Narrator => "narrator",
            Self::Series => "series",
            Self::AudiologoStatus => "audiologo_status",
        }
    }
}

/// Cap on bucket count before the long tail rolls into `Others`.
pub const TOP_N: usize = 20;

fn year_start_unix() -> i64 {
    use chrono::{Datelike, NaiveDate, TimeZone, Utc};
    let now = Utc::now();
    let start = NaiveDate::from_ymd_opt(now.year(), 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| Utc.from_utc_datetime(&dt));
    start.map_or(0, |dt| dt.timestamp())
}

/// Compute counts + listening totals.
pub async fn stats(pool: &SqlitePool) -> Result<StatsResponse, StatsError> {
    let year_start = year_start_unix();

    let counts_row = sqlx::query!(
        r#"SELECT
            COUNT(*) AS "total!: i64",
            SUM(CASE WHEN reading_status = 'want_to_read'
                THEN 1 ELSE 0 END) AS "unread!: i64",
            SUM(CASE WHEN reading_status = 'reading'
                THEN 1 ELSE 0 END) AS "reading!: i64",
            SUM(CASE WHEN reading_status = 'finished'
                THEN 1 ELSE 0 END) AS "finished!: i64",
            SUM(CASE WHEN reading_status = 'dnf'
                THEN 1 ELSE 0 END) AS "dnf!: i64"
         FROM books WHERE deleted_at IS NULL"#,
    )
    .fetch_one(pool)
    .await?;

    let finished_year_row = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64"
         FROM books b
         INNER JOIN media_progress mp ON mp.book_id = b.book_id
         WHERE b.deleted_at IS NULL
           AND b.reading_status = 'finished'
           AND mp.last_listened_at >= ?"#,
        year_start,
    )
    .fetch_one(pool)
    .await?;

    let hours_row = sqlx::query!(
        r#"SELECT
            COALESCE(SUM(CASE WHEN b.reading_status = 'finished'
                THEN b.duration_ms ELSE 0 END), 0) AS "hours_all_time_ms!: i64",
            COALESCE(SUM(CASE WHEN b.reading_status = 'finished'
                AND mp.last_listened_at >= ?
                THEN b.duration_ms ELSE 0 END), 0) AS "hours_year_ms!: i64",
            COALESCE(AVG(CASE WHEN b.reading_status = 'finished'
                THEN b.duration_ms END), 0) AS "avg_finished_ms!: f64"
         FROM books b
         LEFT JOIN media_progress mp ON mp.book_id = b.book_id
         WHERE b.deleted_at IS NULL"#,
        year_start,
    )
    .fetch_one(pool)
    .await?;

    let counts = Counts {
        total: u64::try_from(counts_row.total).unwrap_or(0),
        finished_year: u64::try_from(finished_year_row.n).unwrap_or(0),
        finished_all_time: u64::try_from(counts_row.finished).unwrap_or(0),
        unread: u64::try_from(counts_row.unread).unwrap_or(0),
        reading: u64::try_from(counts_row.reading).unwrap_or(0),
        dnf: u64::try_from(counts_row.dnf).unwrap_or(0),
    };
    let listening = Listening {
        hours_year: ms_to_hours(hours_row.hours_year_ms),
        hours_all_time: ms_to_hours(hours_row.hours_all_time_ms),
        longest_streak_days: 0,
        avg_book_hours: hours_row.avg_finished_ms / 3_600_000.0,
        avg_daily_minutes: 0.0,
    };
    Ok(StatsResponse { counts, listening })
}

#[allow(
    clippy::cast_precision_loss,
    reason = "ms values fit f64 precision for all realistic audiobook durations"
)]
fn ms_to_hours(ms: i64) -> f64 {
    (ms as f64) / 3_600_000.0
}

/// Pie-chart data for `dimension`. Long-tail values beyond
/// [`TOP_N`] aggregate into a single "Others" bucket.
pub async fn breakdown(
    pool: &SqlitePool,
    dimension: Dimension,
) -> Result<BreakdownResponse, StatsError> {
    let raw = match dimension {
        Dimension::Language => language_breakdown(pool).await?,
        Dimension::Length => length_breakdown(pool).await?,
        Dimension::ReadingStatus => reading_status_breakdown(pool).await?,
        Dimension::AcquisitionYear => acquisition_year_breakdown(pool).await?,
        Dimension::Decade => decade_breakdown(pool).await?,
        Dimension::Publisher => publisher_breakdown(pool).await?,
        Dimension::Format => format_breakdown(pool).await?,
        Dimension::Author => author_breakdown(pool).await?,
        Dimension::Narrator => narrator_breakdown(pool).await?,
        Dimension::Series => series_breakdown(pool).await?,
        Dimension::AudiologoStatus => audiologo_status_breakdown(pool).await?,
    };
    let buckets = collapse_long_tail(raw);
    let buckets = with_percentages(buckets);
    Ok(BreakdownResponse {
        dimension: dimension.as_str().to_owned(),
        buckets,
    })
}

struct RawBucket {
    label: String,
    count: i64,
    hours_ms: i64,
}

async fn language_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            COALESCE(language, 'unknown') AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(duration_ms), 0) AS "hours_ms!: i64"
         FROM books
         WHERE deleted_at IS NULL
         GROUP BY language
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn length_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            CASE
                WHEN duration_ms IS NULL THEN 'unknown'
                WHEN duration_ms <  4 * 3600000 THEN '<4h'
                WHEN duration_ms <  8 * 3600000 THEN '4-8h'
                WHEN duration_ms < 15 * 3600000 THEN '8-15h'
                WHEN duration_ms < 25 * 3600000 THEN '15-25h'
                ELSE '25h+'
            END AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(duration_ms), 0) AS "hours_ms!: i64"
         FROM books
         WHERE deleted_at IS NULL
         GROUP BY 1
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn reading_status_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            reading_status AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(duration_ms), 0) AS "hours_ms!: i64"
         FROM books
         WHERE deleted_at IS NULL
         GROUP BY reading_status
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn acquisition_year_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            strftime('%Y', created_at, 'unixepoch') AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(duration_ms), 0) AS "hours_ms!: i64"
         FROM books
         WHERE deleted_at IS NULL
         GROUP BY 1
         ORDER BY 1 DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

/// Group books by release-date decade. `books.release_date` is an
/// ISO 8601 date string (`YYYY-MM-DD`) when Audnexus / tag-read
/// populated it; rows where it's NULL or the first 4 chars don't
/// parse as a year fall into the `unknown` bucket.
///
/// Decade label format: `2010s`, `1990s`, etc. — the SQL
/// `substr(release_date, 1, 3) || '0s'` trick avoids needing a
/// CAST + arithmetic round, and works for any 4-digit YYYY
/// prefix.
async fn decade_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            CASE
                WHEN release_date IS NULL THEN 'unknown'
                WHEN length(release_date) < 4 THEN 'unknown'
                WHEN CAST(substr(release_date, 1, 4) AS INTEGER) = 0 THEN 'unknown'
                ELSE substr(release_date, 1, 3) || '0s'
            END AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(duration_ms), 0) AS "hours_ms!: i64"
         FROM books
         WHERE deleted_at IS NULL
         GROUP BY 1
         ORDER BY 1 DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn publisher_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            COALESCE(p.name, 'unknown') AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(b.duration_ms), 0) AS "hours_ms!: i64"
         FROM books b
         LEFT JOIN publishers p ON p.publisher_id = b.publisher_id
         WHERE b.deleted_at IS NULL
         GROUP BY b.publisher_id
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn format_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            COALESCE(LOWER(bf.format), 'unknown') AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(b.duration_ms), 0) AS "hours_ms!: i64"
         FROM books b
         LEFT JOIN (
             SELECT book_id, MIN(file_id) AS file_id
             FROM book_files
             WHERE is_active = 1
             GROUP BY book_id
         ) first ON first.book_id = b.book_id
         LEFT JOIN book_files bf ON bf.file_id = first.file_id
         WHERE b.deleted_at IS NULL
         GROUP BY LOWER(bf.format)
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn author_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            COALESCE(a.name, 'unknown') AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(b.duration_ms), 0) AS "hours_ms!: i64"
         FROM books b
         LEFT JOIN authors a ON a.author_id = b.author_id
         WHERE b.deleted_at IS NULL
         GROUP BY b.author_id
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn narrator_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            COALESCE(n.name, 'unknown') AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(b.duration_ms), 0) AS "hours_ms!: i64"
         FROM books b
         LEFT JOIN book_narrator bn ON bn.book_id = b.book_id
         LEFT JOIN narrators n ON n.narrator_id = bn.narrator_id
         WHERE b.deleted_at IS NULL
         GROUP BY n.narrator_id
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn series_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            COALESCE(s.name, 'unknown') AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(b.duration_ms), 0) AS "hours_ms!: i64"
         FROM books b
         LEFT JOIN book_series bs ON bs.book_id = b.book_id
         LEFT JOIN series s ON s.series_id = bs.series_id
         WHERE b.deleted_at IS NULL
         GROUP BY s.series_id
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

async fn audiologo_status_breakdown(pool: &SqlitePool) -> Result<Vec<RawBucket>, StatsError> {
    let rows = sqlx::query!(
        r#"SELECT
            audiologo_status AS "bucket_label!: String",
            COUNT(*) AS "n!: i64",
            COALESCE(SUM(duration_ms), 0) AS "hours_ms!: i64"
         FROM books
         WHERE deleted_at IS NULL
         GROUP BY audiologo_status
         ORDER BY COUNT(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RawBucket {
            label: r.bucket_label,
            count: r.n,
            hours_ms: r.hours_ms,
        })
        .collect())
}

fn collapse_long_tail(mut raw: Vec<RawBucket>) -> Vec<Bucket> {
    if raw.len() <= TOP_N {
        return raw
            .into_iter()
            .map(|r| Bucket {
                label: r.label,
                count: u64::try_from(r.count).unwrap_or(0),
                hours: ms_to_hours(r.hours_ms),
                percentage: 0.0,
            })
            .collect();
    }
    let tail = raw.split_off(TOP_N);
    let mut head: Vec<Bucket> = raw
        .into_iter()
        .map(|r| Bucket {
            label: r.label,
            count: u64::try_from(r.count).unwrap_or(0),
            hours: ms_to_hours(r.hours_ms),
            percentage: 0.0,
        })
        .collect();
    let others_count: i64 = tail.iter().map(|r| r.count).sum();
    let others_ms: i64 = tail.iter().map(|r| r.hours_ms).sum();
    head.push(Bucket {
        label: "Others".to_owned(),
        count: u64::try_from(others_count).unwrap_or(0),
        hours: ms_to_hours(others_ms),
        percentage: 0.0,
    });
    head
}

#[allow(
    clippy::cast_precision_loss,
    reason = "bucket counts fit f32 precision for any plausible library size"
)]
fn with_percentages(mut buckets: Vec<Bucket>) -> Vec<Bucket> {
    let total: u64 = buckets.iter().map(|b| b.count).sum();
    if total == 0 {
        return buckets;
    }
    let total_f = total as f32;
    for b in &mut buckets {
        b.percentage = (b.count as f32 / total_f) * 100.0;
    }
    buckets
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use ab_core::tunables::DbTunables;
    use ab_db::LibraryDb;
    use tempfile::TempDir;

    async fn open_db() -> (TempDir, LibraryDb) {
        let dir = TempDir::new().expect("tempdir");
        let lib = LibraryDb::open(&dir.path().join("library.db"), &DbTunables::default())
            .await
            .expect("open library");
        (dir, lib)
    }

    async fn add_book(
        db: &LibraryDb,
        title: &str,
        lang: Option<&str>,
        duration_ms: Option<i64>,
        status: &str,
    ) -> i64 {
        let id = sqlx::query!("INSERT INTO books (title) VALUES (?)", title)
            .execute(db.pool())
            .await
            .expect("insert book")
            .last_insert_rowid();
        if let Some(l) = lang {
            sqlx::query!("UPDATE books SET language = ? WHERE book_id = ?", l, id)
                .execute(db.pool())
                .await
                .expect("set lang");
        }
        if let Some(d) = duration_ms {
            sqlx::query!("UPDATE books SET duration_ms = ? WHERE book_id = ?", d, id)
                .execute(db.pool())
                .await
                .expect("set duration");
        }
        sqlx::query!(
            "UPDATE books SET reading_status = ? WHERE book_id = ?",
            status,
            id,
        )
        .execute(db.pool())
        .await
        .expect("set status");
        id
    }

    #[tokio::test]
    async fn empty_library_returns_zeros() {
        let (_d, db) = open_db().await;
        let s = stats(db.pool()).await.expect("stats");
        assert_eq!(s.counts.total, 0);
        assert!((s.listening.hours_all_time - 0.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn counts_aggregate_by_reading_status() {
        let (_d, db) = open_db().await;
        let _ = add_book(&db, "A", Some("en"), Some(3_600_000), "finished").await;
        let _ = add_book(&db, "B", Some("en"), Some(7_200_000), "reading").await;
        let _ = add_book(&db, "C", Some("de"), Some(3_600_000), "want_to_read").await;
        let s = stats(db.pool()).await.expect("stats");
        assert_eq!(s.counts.total, 3);
        assert_eq!(s.counts.finished_all_time, 1);
        assert_eq!(s.counts.reading, 1);
        assert_eq!(s.counts.unread, 1);
        // 1 finished book × 1h = 1h all-time.
        assert!((s.listening.hours_all_time - 1.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn language_breakdown_sorts_by_count() {
        let (_d, db) = open_db().await;
        let _ = add_book(&db, "A", Some("en"), Some(3_600_000), "reading").await;
        let _ = add_book(&db, "B", Some("en"), Some(3_600_000), "reading").await;
        let _ = add_book(&db, "C", Some("de"), Some(3_600_000), "reading").await;
        let b = breakdown(db.pool(), Dimension::Language)
            .await
            .expect("breakdown");
        assert_eq!(b.buckets.len(), 2);
        assert_eq!(b.buckets[0].label, "en");
        assert_eq!(b.buckets[0].count, 2);
        assert!((b.buckets[0].percentage - 66.66).abs() < 0.5);
    }

    #[tokio::test]
    async fn decade_buckets_group_by_release_year_prefix() {
        let (_d, db) = open_db().await;
        // Two 2010s + one 1990s + one unknown (NULL release_date).
        let id1 = add_book(&db, "A", Some("en"), Some(1), "reading").await;
        let id2 = add_book(&db, "B", Some("en"), Some(1), "reading").await;
        let id3 = add_book(&db, "C", Some("en"), Some(1), "reading").await;
        let _id4 = add_book(&db, "D", Some("en"), Some(1), "reading").await;
        sqlx::query!(
            "UPDATE books SET release_date = '2012-03-04' WHERE book_id = ?",
            id1,
        )
        .execute(db.pool())
        .await
        .expect("set rd1");
        sqlx::query!(
            "UPDATE books SET release_date = '2019-11-30' WHERE book_id = ?",
            id2,
        )
        .execute(db.pool())
        .await
        .expect("set rd2");
        sqlx::query!(
            "UPDATE books SET release_date = '1995' WHERE book_id = ?",
            id3,
        )
        .execute(db.pool())
        .await
        .expect("set rd3");

        let b = breakdown(db.pool(), Dimension::Decade)
            .await
            .expect("breakdown");
        let counts: std::collections::HashMap<String, u64> = b
            .buckets
            .iter()
            .map(|x| (x.label.clone(), x.count))
            .collect();
        assert_eq!(counts.get("2010s").copied(), Some(2));
        assert_eq!(counts.get("1990s").copied(), Some(1));
        assert_eq!(counts.get("unknown").copied(), Some(1));
    }

    #[tokio::test]
    async fn length_buckets_partition_durations() {
        let (_d, db) = open_db().await;
        let _ = add_book(&db, "Short", Some("en"), Some(2 * 3_600_000), "reading").await;
        let _ = add_book(&db, "Medium", Some("en"), Some(6 * 3_600_000), "reading").await;
        let _ = add_book(&db, "Long", Some("en"), Some(30 * 3_600_000), "reading").await;
        let b = breakdown(db.pool(), Dimension::Length)
            .await
            .expect("breakdown");
        let labels: Vec<&str> = b.buckets.iter().map(|x| x.label.as_str()).collect();
        assert!(labels.contains(&"<4h"));
        assert!(labels.contains(&"4-8h"));
        assert!(labels.contains(&"25h+"));
    }

    #[tokio::test]
    async fn publisher_buckets_join_publishers_table() {
        let (_d, db) = open_db().await;
        let p_audible = sqlx::query!("INSERT INTO publishers (name) VALUES ('Audible Studios')")
            .execute(db.pool())
            .await
            .expect("insert publisher audible")
            .last_insert_rowid();
        let p_random = sqlx::query!("INSERT INTO publishers (name) VALUES ('Random House')")
            .execute(db.pool())
            .await
            .expect("insert publisher random")
            .last_insert_rowid();
        let id1 = add_book(&db, "A", Some("en"), Some(1), "reading").await;
        let id2 = add_book(&db, "B", Some("en"), Some(1), "reading").await;
        let id3 = add_book(&db, "C", Some("en"), Some(1), "reading").await;
        let _id4 = add_book(&db, "D", Some("en"), Some(1), "reading").await;
        sqlx::query!(
            "UPDATE books SET publisher_id = ? WHERE book_id = ?",
            p_audible,
            id1,
        )
        .execute(db.pool())
        .await
        .expect("set p1");
        sqlx::query!(
            "UPDATE books SET publisher_id = ? WHERE book_id = ?",
            p_audible,
            id2,
        )
        .execute(db.pool())
        .await
        .expect("set p2");
        sqlx::query!(
            "UPDATE books SET publisher_id = ? WHERE book_id = ?",
            p_random,
            id3,
        )
        .execute(db.pool())
        .await
        .expect("set p3");

        let b = breakdown(db.pool(), Dimension::Publisher)
            .await
            .expect("breakdown");
        let counts: std::collections::HashMap<String, u64> = b
            .buckets
            .iter()
            .map(|x| (x.label.clone(), x.count))
            .collect();
        assert_eq!(counts.get("Audible Studios").copied(), Some(2));
        assert_eq!(counts.get("Random House").copied(), Some(1));
        assert_eq!(counts.get("unknown").copied(), Some(1));
    }

    async fn add_file(db: &LibraryDb, book_id: i64, path: &str, format: Option<&str>, active: i64) {
        sqlx::query!(
            "INSERT INTO book_files (book_id, file_path, format, is_active)
             VALUES (?, ?, ?, ?)",
            book_id,
            path,
            format,
            active,
        )
        .execute(db.pool())
        .await
        .expect("insert file");
    }

    #[tokio::test]
    async fn format_buckets_take_first_active_file_per_book() {
        let (_d, db) = open_db().await;
        let id1 = add_book(&db, "A", Some("en"), Some(1), "reading").await;
        let id2 = add_book(&db, "B", Some("en"), Some(1), "reading").await;
        let id3 = add_book(&db, "C", Some("en"), Some(1), "reading").await;
        let _id4 = add_book(&db, "D", Some("en"), Some(1), "reading").await;

        add_file(&db, id1, "/a/1.m4b", Some("M4B"), 1).await;
        add_file(&db, id2, "/b/1.m4b", Some("m4b"), 1).await;
        add_file(&db, id3, "/c/1.mp3", Some("mp3"), 1).await;
        add_file(&db, id3, "/c/2.mp3", Some("mp3"), 1).await;
        // _id4 has no files → 'unknown'.

        let b = breakdown(db.pool(), Dimension::Format)
            .await
            .expect("breakdown");
        let counts: std::collections::HashMap<String, u64> = b
            .buckets
            .iter()
            .map(|x| (x.label.clone(), x.count))
            .collect();
        // 'M4B' and 'm4b' collapse via LOWER().
        assert_eq!(counts.get("m4b").copied(), Some(2));
        assert_eq!(counts.get("mp3").copied(), Some(1));
        assert_eq!(counts.get("unknown").copied(), Some(1));
    }

    #[test]
    fn dimension_parse_round_trip() {
        for d in [
            Dimension::Language,
            Dimension::Length,
            Dimension::ReadingStatus,
            Dimension::AcquisitionYear,
            Dimension::Decade,
            Dimension::Publisher,
            Dimension::Format,
            Dimension::Author,
            Dimension::Narrator,
            Dimension::Series,
            Dimension::AudiologoStatus,
        ] {
            assert_eq!(Dimension::parse(d.as_str()).expect("parse"), d);
        }
        assert!(Dimension::parse("bogus").is_err());
    }

    #[tokio::test]
    async fn author_buckets_join_authors_table() {
        let (_d, db) = open_db().await;
        let a_king = sqlx::query!("INSERT INTO authors (name) VALUES ('Stephen King')")
            .execute(db.pool())
            .await
            .expect("insert author king")
            .last_insert_rowid();
        let a_atwood = sqlx::query!("INSERT INTO authors (name) VALUES ('Margaret Atwood')")
            .execute(db.pool())
            .await
            .expect("insert author atwood")
            .last_insert_rowid();
        let id1 = add_book(&db, "A", Some("en"), Some(1), "reading").await;
        let id2 = add_book(&db, "B", Some("en"), Some(1), "reading").await;
        let id3 = add_book(&db, "C", Some("en"), Some(1), "reading").await;
        let _id4 = add_book(&db, "D", Some("en"), Some(1), "reading").await;
        sqlx::query!(
            "UPDATE books SET author_id = ? WHERE book_id = ?",
            a_king,
            id1,
        )
        .execute(db.pool())
        .await
        .expect("set a1");
        sqlx::query!(
            "UPDATE books SET author_id = ? WHERE book_id = ?",
            a_king,
            id2,
        )
        .execute(db.pool())
        .await
        .expect("set a2");
        sqlx::query!(
            "UPDATE books SET author_id = ? WHERE book_id = ?",
            a_atwood,
            id3,
        )
        .execute(db.pool())
        .await
        .expect("set a3");

        let b = breakdown(db.pool(), Dimension::Author)
            .await
            .expect("breakdown");
        let counts: std::collections::HashMap<String, u64> = b
            .buckets
            .iter()
            .map(|x| (x.label.clone(), x.count))
            .collect();
        assert_eq!(counts.get("Stephen King").copied(), Some(2));
        assert_eq!(counts.get("Margaret Atwood").copied(), Some(1));
        assert_eq!(counts.get("unknown").copied(), Some(1));
    }

    #[tokio::test]
    async fn narrator_buckets_count_book_narrator_edges() {
        let (_d, db) = open_db().await;
        let doe_id = sqlx::query!("INSERT INTO narrators (name) VALUES ('Jane Doe')")
            .execute(db.pool())
            .await
            .expect("insert narrator doe")
            .last_insert_rowid();
        let roe_id = sqlx::query!("INSERT INTO narrators (name) VALUES ('John Roe')")
            .execute(db.pool())
            .await
            .expect("insert narrator roe")
            .last_insert_rowid();
        let id1 = add_book(&db, "A", Some("en"), Some(1), "reading").await;
        let id2 = add_book(&db, "B", Some("en"), Some(1), "reading").await;
        let id3 = add_book(&db, "C", Some("en"), Some(1), "reading").await;
        let _id4 = add_book(&db, "D", Some("en"), Some(1), "reading").await;
        // id1: solo Doe. id2: full-cast Doe + Roe (counts in both buckets).
        // id3: solo Roe. id4: no narrator → unknown.
        for (b, n) in [(id1, doe_id), (id2, doe_id), (id2, roe_id), (id3, roe_id)] {
            sqlx::query!(
                "INSERT INTO book_narrator (book_id, narrator_id) VALUES (?, ?)",
                b,
                n,
            )
            .execute(db.pool())
            .await
            .expect("insert book_narrator");
        }

        let b = breakdown(db.pool(), Dimension::Narrator)
            .await
            .expect("breakdown");
        let counts: std::collections::HashMap<String, u64> = b
            .buckets
            .iter()
            .map(|x| (x.label.clone(), x.count))
            .collect();
        // Doe narrates id1 + id2 → 2. Roe narrates id2 + id3 → 2. id4 → unknown (1).
        // Sum of bucket counts (5) > total books (4) — the multi-narrator
        // over-count documented on Dimension::Narrator.
        assert_eq!(counts.get("Jane Doe").copied(), Some(2));
        assert_eq!(counts.get("John Roe").copied(), Some(2));
        assert_eq!(counts.get("unknown").copied(), Some(1));
    }

    #[tokio::test]
    async fn series_buckets_count_book_series_edges() {
        let (_d, db) = open_db().await;
        let mistborn = sqlx::query!("INSERT INTO series (name) VALUES ('Mistborn')")
            .execute(db.pool())
            .await
            .expect("insert series mistborn")
            .last_insert_rowid();
        let cosmere = sqlx::query!("INSERT INTO series (name) VALUES ('Cosmere')")
            .execute(db.pool())
            .await
            .expect("insert series cosmere")
            .last_insert_rowid();
        let id1 = add_book(&db, "A", Some("en"), Some(1), "reading").await;
        let id2 = add_book(&db, "B", Some("en"), Some(1), "reading").await;
        let id3 = add_book(&db, "C", Some("en"), Some(1), "reading").await;
        let _id4 = add_book(&db, "D", Some("en"), Some(1), "reading").await;
        // id1: Mistborn only. id2: Mistborn + Cosmere (in both buckets).
        // id3: Cosmere only. id4: standalone → unknown.
        for (b, s) in [
            (id1, mistborn),
            (id2, mistborn),
            (id2, cosmere),
            (id3, cosmere),
        ] {
            sqlx::query!(
                "INSERT INTO book_series (book_id, series_id) VALUES (?, ?)",
                b,
                s,
            )
            .execute(db.pool())
            .await
            .expect("insert book_series");
        }

        let b = breakdown(db.pool(), Dimension::Series)
            .await
            .expect("breakdown");
        let counts: std::collections::HashMap<String, u64> = b
            .buckets
            .iter()
            .map(|x| (x.label.clone(), x.count))
            .collect();
        // Mistborn catalogs id1 + id2 → 2. Cosmere catalogs id2 + id3 → 2.
        // id4 standalone → unknown (1). Sum 5 > total books 4 — multi-series
        // over-count semantic documented on Dimension::Series.
        assert_eq!(counts.get("Mistborn").copied(), Some(2));
        assert_eq!(counts.get("Cosmere").copied(), Some(2));
        assert_eq!(counts.get("unknown").copied(), Some(1));
    }

    #[tokio::test]
    async fn audiologo_status_buckets_count_per_status_value() {
        let (_d, db) = open_db().await;
        // Seed a mix of statuses. add_book leaves audiologo_status
        // at the DEFAULT 'unknown', so we UPDATE explicitly per row.
        let id1 = add_book(&db, "A", Some("en"), Some(1), "reading").await;
        let id2 = add_book(&db, "B", Some("en"), Some(1), "reading").await;
        let id3 = add_book(&db, "C", Some("en"), Some(1), "reading").await;
        let _id4 = add_book(&db, "D", Some("en"), Some(1), "reading").await; // leaves unknown
        for (id, status) in [(id1, "applied"), (id2, "applied"), (id3, "detected")] {
            sqlx::query!(
                "UPDATE books SET audiologo_status = ? WHERE book_id = ?",
                status,
                id,
            )
            .execute(db.pool())
            .await
            .expect("set audiologo_status");
        }
        let b = breakdown(db.pool(), Dimension::AudiologoStatus)
            .await
            .expect("breakdown");
        let counts: std::collections::HashMap<String, u64> = b
            .buckets
            .iter()
            .map(|x| (x.label.clone(), x.count))
            .collect();
        assert_eq!(counts.get("applied").copied(), Some(2));
        assert_eq!(counts.get("detected").copied(), Some(1));
        assert_eq!(counts.get("unknown").copied(), Some(1));
    }
}
