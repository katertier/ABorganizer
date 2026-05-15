//! Language code normalization.
//!
//! Every source that writes a `language` candidate to
//! `book_field_provenance` (read-tags MP4/ID3, Audnexus, Audible,
//! `NLLanguageRecognizer`) emits in a different format:
//!
//! - read-tags MP4 `©lng` / ID3 `TLAN`: ISO-639-2 (`"eng"`,
//!   `"deu"`, `"fra"`)
//! - Audnexus: usually full English name (`"English"`)
//! - Audible API: ISO-639-3 or BCP-47
//! - `NLLanguageRecognizer`: BCP-47-ish primary subtag
//!   (`"en"`, `"de"`, `"zh-Hans"`)
//!
//! Without normalization the consensus stage treats `"en"`,
//! `"eng"`, and `"English"` as three different values and can't
//! aggregate confidence across sources. This module is the
//! single normalize-on-write point.
//!
//! ## Canonical form
//!
//! BCP-47 primary subtag, lowercased, with script-tag preserved
//! when meaningful (`"zh-Hans"`, `"zh-Hant"`). Region is dropped
//! (`"en-US"` → `"en"`) — region matters for picking a
//! `SpeechTranscriber` locale at runtime but isn't a property of
//! the book itself.
//!
//! ## Coverage
//!
//! The mapping table covers ~25 languages: the ABorganizer
//! target audience plus the languages where Apple
//! Intelligence's Speech model has installable assets. Adding
//! entries is cheap; extend the table when real data shows
//! up for a missing language.

use std::sync::OnceLock;

/// Canonicalise an arbitrary language code / name.
///
/// Output is the project's preferred form: BCP-47 primary
/// subtag, lowercased, script preserved when present. Returns
/// `None` when the input doesn't match any known mapping.
///
/// Behaviour:
///
/// - Whitespace-trimmed, case-insensitive on input.
/// - ISO-639-1 (`"en"`, `"de"`) → returned as-is, lowercased.
/// - ISO-639-2/T and 639-2/B (`"eng"`, `"deu"`, `"ger"`,
///   `"fra"`, `"fre"`) → mapped to the 639-1 form.
/// - ISO-639-3 (`"eng"`, `"deu"`) → same.
/// - BCP-47 with region (`"en-US"`, `"de-AT"`) → primary subtag
///   only (`"en"`, `"de"`).
/// - BCP-47 with script (`"zh-Hans"`, `"zh-Hant"`) → kept as
///   primary-Script (`"zh-Hans"`).
/// - English language names (`"English"`, `"German"`,
///   `"Mandarin"`) → mapped to the canonical code.
/// - Empty / unparseable input → `None`.
///
/// # Examples
///
/// ```
/// use ab_core::language_code::normalize;
/// assert_eq!(normalize("en").as_deref(), Some("en"));
/// assert_eq!(normalize("eng").as_deref(), Some("en"));
/// assert_eq!(normalize("en-US").as_deref(), Some("en"));
/// assert_eq!(normalize("English").as_deref(), Some("en"));
/// assert_eq!(normalize("zh-Hans").as_deref(), Some("zh-Hans"));
/// assert_eq!(normalize("klingon"), None);
/// ```
#[must_use]
pub fn normalize(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Look for a script-tag suffix (e.g. `zh-Hans` / `zh-Hant`)
    // before lowercasing — Script subtags are case-sensitive
    // in BCP-47 (`Hans` not `hans`). Match on the primary
    // subtag's lowercased form.
    if let Some(canonical) = match_script_form(trimmed) {
        return Some(canonical.to_owned());
    }
    let lower = trimmed.to_ascii_lowercase();
    // Strip a region suffix (`en-us`, `de-at`) — anything after
    // the first `-` is the region; the primary subtag is what we
    // want.
    let primary = lower.split('-').next().unwrap_or(&lower);
    for entry in mapping_table() {
        if entry
            .aliases
            .iter()
            .any(|a| a.eq_ignore_ascii_case(primary))
        {
            return Some(entry.canonical.to_owned());
        }
    }
    None
}

/// Locale-aware display name for a canonical code.
///
/// `display_locale` selects the language the name should be
/// rendered in: `"en"` returns English names (`"German"`),
/// `"de"` returns German names (`"Deutsch"`), `"fr"` returns
/// French (`"Allemand"`), etc. Falls back to English when the
/// requested locale isn't in the localised-names table.
/// Returns `"Unknown"` for canonical codes the table doesn't
/// carry.
///
/// # Examples
///
/// ```
/// use ab_core::language_code::display_name;
/// assert_eq!(display_name("en", "en"), "English");
/// assert_eq!(display_name("de", "en"), "German");
/// assert_eq!(display_name("de", "de"), "Deutsch");
/// assert_eq!(display_name("en", "de"), "Englisch");
/// assert_eq!(display_name("zh-Hans", "en"), "Mandarin (Simplified)");
/// assert_eq!(display_name("klingon", "en"), "Unknown");
/// ```
#[must_use]
pub fn display_name(canonical: &str, display_locale: &str) -> &'static str {
    let locale_short = display_locale.split('-').next().unwrap_or(display_locale);
    for entry in mapping_table() {
        if !entry.canonical.eq_ignore_ascii_case(canonical) {
            continue;
        }
        // Look up the requested display locale; fall through to
        // English when the table doesn't carry a localised
        // string for it.
        for (loc, name) in entry.display_localized {
            if loc.eq_ignore_ascii_case(locale_short) {
                return name;
            }
        }
        return entry.display;
    }
    "Unknown"
}

/// One entry in the static mapping table.
struct Entry {
    /// Canonical form written to `book_field_provenance.value`.
    canonical: &'static str,
    /// English display name — used as the fallback when the
    /// caller's `display_locale` isn't in `display_localized`.
    display: &'static str,
    /// Locale → display-name pairs. Looked up before falling
    /// back to `display`. Locale match is on the primary subtag
    /// (`"de"` matches `"de-DE"`, `"de-AT"`).
    display_localized: &'static [(&'static str, &'static str)],
    /// All accepted input forms (primary subtag form only — the
    /// caller already stripped region suffixes). Case-insensitive
    /// compare. The canonical form is implicitly in the list
    /// (always matches itself); explicit aliases cover the other
    /// shapes (ISO-639-1, -2, -3, English name).
    aliases: &'static [&'static str],
}

// The mapping table is intentionally long — that's just data,
// not branching logic. clippy::too_many_lines fires because the
// fn body is ~130 lines; suppressing here keeps the table dense
// instead of fanned out across N constructor helpers.
#[expect(
    clippy::too_many_lines,
    reason = "static language mapping table; growing it shouldn't trigger reformat"
)]
fn mapping_table() -> &'static [Entry] {
    static T: OnceLock<Vec<Entry>> = OnceLock::new();
    T.get_or_init(|| {
        // Localised display names cover en/de/fr/es as the
        // current UI locales. Each Entry only carries the
        // localisations that ship; other locales fall back to
        // the English `display` field. Future locales add a row
        // per entry without changing call sites.
        vec![
            Entry {
                canonical: "en",
                display: "English",
                display_localized: &[("de", "Englisch"), ("fr", "Anglais"), ("es", "Inglés")],
                aliases: &["en", "eng", "english"],
            },
            Entry {
                canonical: "de",
                display: "German",
                display_localized: &[("de", "Deutsch"), ("fr", "Allemand"), ("es", "Alemán")],
                aliases: &["de", "deu", "ger", "german", "deutsch"],
            },
            Entry {
                canonical: "fr",
                display: "French",
                display_localized: &[("de", "Französisch"), ("fr", "Français"), ("es", "Francés")],
                aliases: &["fr", "fra", "fre", "french", "français", "francais"],
            },
            Entry {
                canonical: "es",
                display: "Spanish",
                display_localized: &[("de", "Spanisch"), ("fr", "Espagnol"), ("es", "Español")],
                aliases: &["es", "spa", "spanish", "español", "espanol", "castellano"],
            },
            Entry {
                canonical: "it",
                display: "Italian",
                display_localized: &[("de", "Italienisch"), ("fr", "Italien"), ("es", "Italiano")],
                aliases: &["it", "ita", "italian", "italiano"],
            },
            Entry {
                canonical: "pt",
                display: "Portuguese",
                display_localized: &[
                    ("de", "Portugiesisch"),
                    ("fr", "Portugais"),
                    ("es", "Portugués"),
                ],
                aliases: &["pt", "por", "portuguese", "português", "portugues"],
            },
            Entry {
                canonical: "nl",
                display: "Dutch",
                display_localized: &[
                    ("de", "Niederländisch"),
                    ("fr", "Néerlandais"),
                    ("es", "Neerlandés"),
                ],
                aliases: &["nl", "nld", "dut", "dutch", "nederlands"],
            },
            Entry {
                canonical: "sv",
                display: "Swedish",
                display_localized: &[("de", "Schwedisch"), ("fr", "Suédois"), ("es", "Sueco")],
                aliases: &["sv", "swe", "swedish", "svenska"],
            },
            Entry {
                canonical: "no",
                display: "Norwegian",
                display_localized: &[("de", "Norwegisch"), ("fr", "Norvégien"), ("es", "Noruego")],
                aliases: &["no", "nor", "nob", "nno", "norwegian", "norsk"],
            },
            Entry {
                canonical: "da",
                display: "Danish",
                display_localized: &[("de", "Dänisch"), ("fr", "Danois"), ("es", "Danés")],
                aliases: &["da", "dan", "danish", "dansk"],
            },
            Entry {
                canonical: "fi",
                display: "Finnish",
                display_localized: &[("de", "Finnisch"), ("fr", "Finnois"), ("es", "Finés")],
                aliases: &["fi", "fin", "finnish", "suomi"],
            },
            Entry {
                canonical: "is",
                display: "Icelandic",
                display_localized: &[
                    ("de", "Isländisch"),
                    ("fr", "Islandais"),
                    ("es", "Islandés"),
                ],
                aliases: &["is", "isl", "ice", "icelandic", "íslenska", "islenska"],
            },
            Entry {
                canonical: "pl",
                display: "Polish",
                display_localized: &[("de", "Polnisch"), ("fr", "Polonais"), ("es", "Polaco")],
                aliases: &["pl", "pol", "polish", "polski"],
            },
            Entry {
                canonical: "cs",
                display: "Czech",
                display_localized: &[("de", "Tschechisch"), ("fr", "Tchèque"), ("es", "Checo")],
                aliases: &["cs", "ces", "cze", "czech", "čeština", "cestina"],
            },
            Entry {
                canonical: "ru",
                display: "Russian",
                display_localized: &[("de", "Russisch"), ("fr", "Russe"), ("es", "Ruso")],
                aliases: &["ru", "rus", "russian", "русский", "russkij"],
            },
            Entry {
                canonical: "uk",
                display: "Ukrainian",
                display_localized: &[
                    ("de", "Ukrainisch"),
                    ("fr", "Ukrainien"),
                    ("es", "Ucraniano"),
                ],
                aliases: &["uk", "ukr", "ukrainian", "українська"],
            },
            Entry {
                canonical: "tr",
                display: "Turkish",
                display_localized: &[("de", "Türkisch"), ("fr", "Turc"), ("es", "Turco")],
                aliases: &["tr", "tur", "turkish", "türkçe", "turkce"],
            },
            Entry {
                canonical: "ja",
                display: "Japanese",
                display_localized: &[("de", "Japanisch"), ("fr", "Japonais"), ("es", "Japonés")],
                aliases: &["ja", "jpn", "japanese", "日本語"],
            },
            Entry {
                canonical: "ko",
                display: "Korean",
                display_localized: &[("de", "Koreanisch"), ("fr", "Coréen"), ("es", "Coreano")],
                aliases: &["ko", "kor", "korean", "한국어"],
            },
            Entry {
                canonical: "hi",
                display: "Hindi",
                display_localized: &[("de", "Hindi"), ("fr", "Hindi"), ("es", "Hindi")],
                aliases: &["hi", "hin", "hindi", "हिन्दी"],
            },
            Entry {
                canonical: "ar",
                display: "Arabic",
                display_localized: &[("de", "Arabisch"), ("fr", "Arabe"), ("es", "Árabe")],
                aliases: &["ar", "ara", "arabic", "العربية"],
            },
            Entry {
                canonical: "he",
                display: "Hebrew",
                display_localized: &[("de", "Hebräisch"), ("fr", "Hébreu"), ("es", "Hebreo")],
                aliases: &["he", "heb", "iw", "hebrew", "עברית"],
            },
            Entry {
                canonical: "el",
                display: "Greek",
                display_localized: &[("de", "Griechisch"), ("fr", "Grec"), ("es", "Griego")],
                aliases: &["el", "ell", "gre", "greek", "ελληνικά"],
            },
            Entry {
                canonical: "zh-Hans",
                display: "Mandarin (Simplified)",
                display_localized: &[
                    ("de", "Mandarin (Vereinfacht)"),
                    ("fr", "Mandarin (Simplifié)"),
                    ("es", "Mandarín (Simplificado)"),
                ],
                aliases: &[],
            },
            Entry {
                canonical: "zh-Hant",
                display: "Mandarin (Traditional)",
                display_localized: &[
                    ("de", "Mandarin (Traditionell)"),
                    ("fr", "Mandarin (Traditionnel)"),
                    ("es", "Mandarín (Tradicional)"),
                ],
                aliases: &[],
            },
            Entry {
                canonical: "zh",
                display: "Mandarin",
                display_localized: &[("de", "Mandarin"), ("fr", "Mandarin"), ("es", "Mandarín")],
                aliases: &["zh", "zho", "chi", "chinese", "mandarin", "中文"],
            },
        ]
    })
}

/// Match script-form codes (`"zh-Hans"`, `"zh-Hant"`) before
/// general lowercasing. Returns the canonical form when matched,
/// `None` otherwise. The compare on the primary subtag is
/// case-insensitive; the script tag uses BCP-47's standard
/// capitalisation (`"Hans"` not `"hans"` or `"HANS"`).
fn match_script_form(input: &str) -> Option<&'static str> {
    let mut parts = input.split('-');
    let primary = parts.next()?.to_ascii_lowercase();
    let script = parts.next()?;
    // Capitalised script subtag (BCP-47 convention): first char
    // upper, rest lower. We accept any casing on input and
    // normalise to the canonical form.
    let mut script_chars = script.chars();
    let first = script_chars.next()?.to_ascii_uppercase();
    let rest: String = script_chars
        .as_str()
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let script_normalised = format!("{first}{rest}");
    let candidate = format!("{primary}-{script_normalised}");
    for entry in mapping_table() {
        if entry.canonical == candidate {
            return Some(entry.canonical);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_639_1_passes_through() {
        assert_eq!(normalize("en").as_deref(), Some("en"));
        assert_eq!(normalize("de").as_deref(), Some("de"));
        assert_eq!(normalize("ja").as_deref(), Some("ja"));
    }

    #[test]
    fn iso_639_2_mapped() {
        assert_eq!(normalize("eng").as_deref(), Some("en"));
        assert_eq!(normalize("deu").as_deref(), Some("de"));
        assert_eq!(normalize("ger").as_deref(), Some("de"));
        assert_eq!(normalize("fra").as_deref(), Some("fr"));
        assert_eq!(normalize("fre").as_deref(), Some("fr"));
    }

    #[test]
    fn bcp47_region_stripped() {
        assert_eq!(normalize("en-US").as_deref(), Some("en"));
        assert_eq!(normalize("EN-US").as_deref(), Some("en"));
        assert_eq!(normalize("de-AT").as_deref(), Some("de"));
        assert_eq!(normalize("pt-BR").as_deref(), Some("pt"));
    }

    #[test]
    fn bcp47_script_preserved() {
        assert_eq!(normalize("zh-Hans").as_deref(), Some("zh-Hans"));
        assert_eq!(normalize("zh-Hant").as_deref(), Some("zh-Hant"));
        // Casing normalised on input.
        assert_eq!(normalize("ZH-HANS").as_deref(), Some("zh-Hans"));
        assert_eq!(normalize("zh-hans").as_deref(), Some("zh-Hans"));
        // Without script → bare `zh`.
        assert_eq!(normalize("zh").as_deref(), Some("zh"));
        assert_eq!(normalize("chinese").as_deref(), Some("zh"));
    }

    #[test]
    fn english_names_mapped() {
        assert_eq!(normalize("English").as_deref(), Some("en"));
        assert_eq!(normalize("german").as_deref(), Some("de"));
        assert_eq!(normalize("MANDARIN").as_deref(), Some("zh"));
    }

    #[test]
    fn empty_and_unknown() {
        assert!(normalize("").is_none());
        assert!(normalize("   ").is_none());
        assert!(normalize("klingon").is_none());
        assert!(normalize("xx").is_none());
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(normalize("  en  ").as_deref(), Some("en"));
        assert_eq!(normalize("\tEnglish\n").as_deref(), Some("en"));
    }

    #[test]
    fn display_name_english_locale() {
        assert_eq!(display_name("en", "en"), "English");
        assert_eq!(display_name("de", "en"), "German");
        assert_eq!(display_name("zh-Hans", "en"), "Mandarin (Simplified)");
        assert_eq!(display_name("zh-Hant", "en"), "Mandarin (Traditional)");
        assert_eq!(display_name("zh", "en"), "Mandarin");
    }

    #[test]
    fn display_name_german_locale() {
        assert_eq!(display_name("en", "de"), "Englisch");
        assert_eq!(display_name("de", "de"), "Deutsch");
        assert_eq!(display_name("fr", "de"), "Französisch");
        assert_eq!(display_name("zh-Hans", "de"), "Mandarin (Vereinfacht)");
    }

    #[test]
    fn display_name_french_locale() {
        assert_eq!(display_name("en", "fr"), "Anglais");
        assert_eq!(display_name("de", "fr"), "Allemand");
        assert_eq!(display_name("ja", "fr"), "Japonais");
    }

    #[test]
    fn display_name_with_region_normalises_to_primary_subtag() {
        // `de-AT` should be treated like `de` for lookup.
        assert_eq!(display_name("de", "de-AT"), "Deutsch");
        assert_eq!(display_name("en", "DE-DE"), "Englisch");
    }

    #[test]
    fn display_name_unknown_locale_falls_back_to_english() {
        // Locale we don't carry → English fallback.
        assert_eq!(display_name("de", "klingon"), "German");
        assert_eq!(display_name("en", "xx"), "English");
    }

    #[test]
    fn display_name_unknown_canonical_returns_unknown() {
        assert_eq!(display_name("klingon", "en"), "Unknown");
        assert_eq!(display_name("", "en"), "Unknown");
        assert_eq!(display_name("klingon", "de"), "Unknown");
    }
}
