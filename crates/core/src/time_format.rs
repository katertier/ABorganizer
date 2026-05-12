//! Locale-aware date / time / number formatting.
//!
//! All UI surfaces (HTML reports, web player, config UI,
//! ABS-compat API output where applicable) route timestamps and
//! numbers through here. The active locale comes from
//! `Tunables.library_display.library_locale`.
//!
//! Pure-Rust hand-coded format strings for v0. Future fidelity
//! bump: route through `CFLocale` / `NSDateFormatter` /
//! `NSNumberFormatter` via Swift FFI for full ICU coverage and
//! every conceivable locale variant. For the target audience —
//! 5 UI locales, 1 currency, no calendar-system gymnastics —
//! hand-coded is enough.
//!
//! ## Date semantics
//!
//! Three styles supported:
//!
//! - [`DateStyle::Short`] — numeric only (`"01/15/2026"`,
//!   `"15.01.2026"`, `"15/01/2026"`). For table cells.
//! - [`DateStyle::Medium`] — month name abbreviated
//!   (`"Jan 15, 2026"`, `"15. Jan. 2026"`). For card summaries.
//! - [`DateStyle::Long`] — full month name
//!   (`"January 15, 2026"`, `"15. Januar 2026"`). For detail
//!   pages.
//!
//! ## Time semantics
//!
//! 24-hour for every supported locale except `"en"`/`"en-US"`
//! which uses 12-hour with `AM`/`PM`. `"en-GB"` uses 24-hour
//! like the rest of Europe.

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike, Utc};

/// Date-format granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateStyle {
    /// Numeric only — e.g. `"01/15/2026"`.
    Short,
    /// Abbreviated month name — e.g. `"Jan 15, 2026"`.
    Medium,
    /// Full month name — e.g. `"January 15, 2026"`.
    Long,
}

/// Format a Unix timestamp (seconds since epoch) as a date in
/// the requested locale + style. Renders in the user's local
/// time zone.
///
/// Returns `"—"` for the sentinel timestamp `0` and any negative
/// values that fall outside chrono's representable range — these
/// would render as `"1970-01-01"` or similar which is confusing
/// rather than informative.
///
/// # Examples
///
/// ```
/// use ab_core::time_format::{format_date, DateStyle};
/// // 2026-01-15 12:00:00 UTC
/// let ts = 1_768_521_600_i64;
/// assert!(format_date(ts, "en", DateStyle::Short).contains("2026"));
/// ```
#[must_use]
pub fn format_date(unix_secs: i64, locale: &str, style: DateStyle) -> String {
    let Some(dt) = local_dt(unix_secs) else {
        return EMPTY_DATE.to_owned();
    };
    let locale_short = primary_subtag(locale);
    match style {
        DateStyle::Short => format_date_short(&dt, locale_short),
        DateStyle::Medium => format_date_medium(&dt, locale_short),
        DateStyle::Long => format_date_long(&dt, locale_short),
    }
}

/// Format a Unix timestamp as a datetime. Style applies to the
/// date portion; the time component renders 24-hour everywhere
/// except `"en"` / `"en-US"` (which use 12-hour with AM/PM).
///
/// # Examples
///
/// ```
/// use ab_core::time_format::{format_datetime, DateStyle};
/// let ts = 1_768_521_600_i64; // 2026-01-15 12:00:00 UTC
/// // Renders date + time per locale; exact wall-clock time
/// // depends on the local TZ.
/// let _ = format_datetime(ts, "de", DateStyle::Short);
/// ```
#[must_use]
pub fn format_datetime(unix_secs: i64, locale: &str, style: DateStyle) -> String {
    let Some(dt) = local_dt(unix_secs) else {
        return EMPTY_DATE.to_owned();
    };
    let locale_short = primary_subtag(locale);
    let date_part = match style {
        DateStyle::Short => format_date_short(&dt, locale_short),
        DateStyle::Medium => format_date_medium(&dt, locale_short),
        DateStyle::Long => format_date_long(&dt, locale_short),
    };
    let time_part = format_time(&dt, locale_short, locale);
    // Locale-specific separator: en/fr/es/it use space + "at" /
    // "à" /"a las"; de uses ", " then "um". For v0 just use a
    // space — close enough; matches most chrono-like formatters.
    format!("{date_part} {time_part}")
}

/// Format an integer with thousand separators per locale.
///
/// # Examples
///
/// ```
/// use ab_core::time_format::format_integer;
/// assert_eq!(format_integer(1_234_567, "en"), "1,234,567");
/// assert_eq!(format_integer(1_234_567, "de"), "1.234.567");
/// // French uses U+202F (narrow no-break space) per ISO 31-0.
/// assert_eq!(format_integer(1_234_567, "fr"), "1\u{202f}234\u{202f}567");
/// ```
#[must_use]
pub fn format_integer(value: i64, locale: &str) -> String {
    let separator = thousands_separator(primary_subtag(locale));
    // Build digit groups right-to-left.
    let mut digits: Vec<char> = value.unsigned_abs().to_string().chars().collect();
    if digits.is_empty() {
        return "0".to_owned();
    }
    let mut out = Vec::with_capacity(digits.len() + digits.len() / 3);
    let mut count = 0;
    while let Some(d) = digits.pop() {
        if count == 3 {
            out.push(separator);
            count = 0;
        }
        out.push(d);
        count += 1;
    }
    out.reverse();
    let body: String = out.into_iter().collect();
    if value < 0 { format!("-{body}") } else { body }
}

/// Format a duration in seconds as `Hh Mm`, `Mm`, or `Ss`
/// depending on length. Locale-agnostic — uses `h`/`m`/`s`
/// suffixes which are recognised across Latin-script locales.
///
/// # Examples
///
/// ```
/// use ab_core::time_format::format_duration_secs;
/// assert_eq!(format_duration_secs(45), "45s");
/// assert_eq!(format_duration_secs(125), "2m 5s");
/// assert_eq!(format_duration_secs(3725), "1h 2m");
/// ```
#[must_use]
pub fn format_duration_secs(total_secs: u64) -> String {
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

// ── Internals ──────────────────────────────────────────────────

/// Sentinel string for "no date" / out-of-range timestamps.
const EMPTY_DATE: &str = "—";

fn primary_subtag(locale: &str) -> &str {
    locale.split('-').next().unwrap_or(locale)
}

fn local_dt(unix_secs: i64) -> Option<DateTime<Local>> {
    if unix_secs == 0 {
        return None;
    }
    let utc: DateTime<Utc> = Utc.timestamp_opt(unix_secs, 0).single()?;
    Some(utc.with_timezone(&Local))
}

fn format_date_short(dt: &DateTime<Local>, locale_short: &str) -> String {
    match locale_short {
        "en" => format!("{:02}/{:02}/{}", dt.month(), dt.day(), dt.year()),
        "de" => format!("{:02}.{:02}.{}", dt.day(), dt.month(), dt.year()),
        "fr" | "es" | "it" => format!("{:02}/{:02}/{}", dt.day(), dt.month(), dt.year()),
        _ => format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day()),
    }
}

fn format_date_medium(dt: &DateTime<Local>, locale_short: &str) -> String {
    let month = month_name_short(dt.month(), locale_short);
    // Locale-specific medium patterns. The `_` wildcard
    // covers fr/es/it AND any non-table locale — they share
    // `day month year` ordering. Keeping them collapsed is
    // intentional, not duplication.
    match locale_short {
        "en" => format!("{} {}, {}", month, dt.day(), dt.year()),
        "de" => format!("{}. {} {}", dt.day(), month, dt.year()),
        _ => format!("{} {} {}", dt.day(), month, dt.year()),
    }
}

fn format_date_long(dt: &DateTime<Local>, locale_short: &str) -> String {
    let month = month_name_long(dt.month(), locale_short);
    match locale_short {
        "en" => format!("{} {}, {}", month, dt.day(), dt.year()),
        "de" => format!("{}. {} {}", dt.day(), month, dt.year()),
        // Spanish long form uses connectives — "15 de julio de
        // 2026". Other Latin-script locales (fr / it / fallback)
        // use the day-month-year pattern without connectives.
        "es" => format!("{} de {} de {}", dt.day(), month, dt.year()),
        _ => format!("{} {} {}", dt.day(), month, dt.year()),
    }
}

fn format_time(dt: &DateTime<Local>, locale_short: &str, full_locale: &str) -> String {
    // `en-US` is 12-hour with AM/PM. `en-GB` is 24-hour. The
    // primary-subtag-only path keeps en defaulting to en-US
    // since that's the most-common UI locale assumption.
    let twelve_hour = locale_short == "en" && !full_locale.eq_ignore_ascii_case("en-GB");
    if twelve_hour {
        let hour12 = match dt.hour() {
            0 => 12,
            h if h > 12 => h - 12,
            h => h,
        };
        let suffix = if dt.hour() < 12 { "AM" } else { "PM" };
        format!("{hour12}:{:02} {suffix}", dt.minute())
    } else {
        format!("{:02}:{:02}", dt.hour(), dt.minute())
    }
}

fn thousands_separator(locale_short: &str) -> char {
    // en + fallback both use ',' — collapsed. Continental
    // European locales use '.' or NNBSP per their conventions.
    match locale_short {
        "de" | "it" | "es" | "nl" | "pt" => '.',
        "fr" => '\u{202f}', // narrow no-break space, French convention
        _ => ',',
    }
}

fn month_name_short(month: u32, locale: &str) -> &'static str {
    match locale {
        "de" => DE_MONTHS_SHORT[(month as usize).saturating_sub(1).min(11)],
        "fr" => FR_MONTHS_SHORT[(month as usize).saturating_sub(1).min(11)],
        "es" => ES_MONTHS_SHORT[(month as usize).saturating_sub(1).min(11)],
        "it" => IT_MONTHS_SHORT[(month as usize).saturating_sub(1).min(11)],
        _ => EN_MONTHS_SHORT[(month as usize).saturating_sub(1).min(11)],
    }
}

fn month_name_long(month: u32, locale: &str) -> &'static str {
    match locale {
        "de" => DE_MONTHS_LONG[(month as usize).saturating_sub(1).min(11)],
        "fr" => FR_MONTHS_LONG[(month as usize).saturating_sub(1).min(11)],
        "es" => ES_MONTHS_LONG[(month as usize).saturating_sub(1).min(11)],
        "it" => IT_MONTHS_LONG[(month as usize).saturating_sub(1).min(11)],
        _ => EN_MONTHS_LONG[(month as usize).saturating_sub(1).min(11)],
    }
}

const EN_MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const EN_MONTHS_LONG: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

const DE_MONTHS_SHORT: [&str; 12] = [
    "Jan.", "Feb.", "März", "Apr.", "Mai", "Juni", "Juli", "Aug.", "Sep.", "Okt.", "Nov.", "Dez.",
];
const DE_MONTHS_LONG: [&str; 12] = [
    "Januar",
    "Februar",
    "März",
    "April",
    "Mai",
    "Juni",
    "Juli",
    "August",
    "September",
    "Oktober",
    "November",
    "Dezember",
];

const FR_MONTHS_SHORT: [&str; 12] = [
    "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.",
    "déc.",
];
const FR_MONTHS_LONG: [&str; 12] = [
    "janvier",
    "février",
    "mars",
    "avril",
    "mai",
    "juin",
    "juillet",
    "août",
    "septembre",
    "octobre",
    "novembre",
    "décembre",
];

const ES_MONTHS_SHORT: [&str; 12] = [
    "ene.", "feb.", "mar.", "abr.", "may.", "jun.", "jul.", "ago.", "sept.", "oct.", "nov.", "dic.",
];
const ES_MONTHS_LONG: [&str; 12] = [
    "enero",
    "febrero",
    "marzo",
    "abril",
    "mayo",
    "junio",
    "julio",
    "agosto",
    "septiembre",
    "octubre",
    "noviembre",
    "diciembre",
];

const IT_MONTHS_SHORT: [&str; 12] = [
    "gen.", "feb.", "mar.", "apr.", "mag.", "giu.", "lug.", "ago.", "set.", "ott.", "nov.", "dic.",
];
const IT_MONTHS_LONG: [&str; 12] = [
    "gennaio",
    "febbraio",
    "marzo",
    "aprile",
    "maggio",
    "giugno",
    "luglio",
    "agosto",
    "settembre",
    "ottobre",
    "novembre",
    "dicembre",
];

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// 2026-01-15 12:00:00 UTC — fixed point for the test
    /// suite. Real wall-clock rendering depends on the host
    /// timezone, so tests assert on substrings rather than
    /// exact strings.
    const FIXED_TS: i64 = 1_768_521_600;

    #[test]
    fn date_short_en() {
        let s = format_date(FIXED_TS, "en", DateStyle::Short);
        // MM/DD/YYYY pattern.
        assert!(s.contains("2026"), "{s}");
        assert!(s.contains('/'), "{s}");
    }

    #[test]
    fn date_short_de() {
        let s = format_date(FIXED_TS, "de", DateStyle::Short);
        // DD.MM.YYYY pattern with dots.
        assert!(s.contains("2026"), "{s}");
        assert!(s.contains('.'), "{s}");
    }

    #[test]
    fn date_medium_german_uses_german_month() {
        // Pick a month that's distinct: 2026-03-15 has month=3
        // which in German short is "März".
        let ts = DateTime::parse_from_rfc3339("2026-03-15T12:00:00+00:00")
            .expect("parse")
            .timestamp();
        let s = format_date(ts, "de", DateStyle::Medium);
        assert!(s.contains("März"), "{s}");
    }

    #[test]
    fn date_long_french() {
        let ts = DateTime::parse_from_rfc3339("2026-07-15T12:00:00+00:00")
            .expect("parse")
            .timestamp();
        let s = format_date(ts, "fr", DateStyle::Long);
        assert!(s.contains("juillet"), "{s}");
    }

    #[test]
    fn date_long_spanish_uses_de_de() {
        // Spanish long form: "15 de julio de 2026"
        let ts = DateTime::parse_from_rfc3339("2026-07-15T12:00:00+00:00")
            .expect("parse")
            .timestamp();
        let s = format_date(ts, "es", DateStyle::Long);
        assert!(s.contains("julio"), "{s}");
        assert!(s.contains(" de "), "{s}");
    }

    #[test]
    fn date_zero_returns_em_dash() {
        assert_eq!(format_date(0, "en", DateStyle::Short), "—");
        assert_eq!(format_date(0, "de", DateStyle::Long), "—");
    }

    #[test]
    fn date_unknown_locale_falls_back_to_iso() {
        let s = format_date(FIXED_TS, "xx", DateStyle::Short);
        // YYYY-MM-DD ISO fallback.
        assert!(s.starts_with("2026-"), "{s}");
    }

    #[test]
    fn integer_thousands() {
        assert_eq!(format_integer(1_234_567, "en"), "1,234,567");
        assert_eq!(format_integer(1_234_567, "de"), "1.234.567");
        assert_eq!(format_integer(1_234_567, "fr"), "1\u{202f}234\u{202f}567");
        assert_eq!(format_integer(0, "en"), "0");
        assert_eq!(format_integer(42, "en"), "42");
    }

    #[test]
    fn integer_negative() {
        assert_eq!(format_integer(-1_234, "en"), "-1,234");
        assert_eq!(format_integer(-1_234, "de"), "-1.234");
    }

    #[test]
    fn duration_formats() {
        assert_eq!(format_duration_secs(45), "45s");
        assert_eq!(format_duration_secs(60), "1m 0s");
        assert_eq!(format_duration_secs(125), "2m 5s");
        assert_eq!(format_duration_secs(3600), "1h 0m");
        assert_eq!(format_duration_secs(3725), "1h 2m");
    }

    #[test]
    fn datetime_includes_both_date_and_time() {
        let s = format_datetime(FIXED_TS, "en", DateStyle::Short);
        assert!(s.contains("2026"), "{s}");
        // English short time has colon between hour + minute.
        assert!(s.contains(':'), "{s}");
    }
}
