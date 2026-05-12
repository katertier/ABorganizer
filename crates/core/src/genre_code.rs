//! Genre normalization + locale-aware display.
//!
//! Mirrors [`crate::language_code`]: arbitrary input (Audnexus
//! `"Science Fiction"`, Audible `"sci-fi"`, tag-read
//! `"Science-Fiction"`) → canonical slug (`"science-fiction"`)
//! → locale-aware display (`"Science Fiction"` in `"en"`,
//! `"Science-Fiction"` in `"de"`, `"Ciencia ficción"` in `"es"`).
//!
//! Storage shape: `book_tags.value` carries the canonical slug
//! prefixed `@` per the project tag convention
//! (`@science-fiction`). The display layer strips the prefix and
//! routes through [`display_name`].
//!
//! ## Scope
//!
//! 30 common audiobook genres covered for v0. Adding entries is
//! cheap. Sub-genres (`urban-fantasy` vs. `fantasy`) collapse to
//! the parent canonical slug for now; a hierarchy is a future
//! slice.

use std::sync::OnceLock;

/// Canonicalise an arbitrary genre name into a slug suitable
/// for the `@<slug>` tag form.
///
/// Output is lowercase, hyphen-separated, with non-alphanumeric
/// characters dropped. Multiple input forms collapse to one
/// slug — `"Science Fiction"`, `"Sci-Fi"`, `"Science-Fiction"`,
/// `"SciFi"` all return `"science-fiction"`.
///
/// Returns `None` for empty / whitespace-only input or for
/// strings that don't match any known genre.
///
/// # Examples
///
/// ```
/// use ab_core::genre_code::normalize;
/// assert_eq!(normalize("Science Fiction").as_deref(), Some("science-fiction"));
/// assert_eq!(normalize("Sci-Fi").as_deref(), Some("science-fiction"));
/// assert_eq!(normalize("FANTASY").as_deref(), Some("fantasy"));
/// assert_eq!(normalize("xxxx-unknown"), None);
/// ```
#[must_use]
pub fn normalize(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let slug = slugify(trimmed);
    if slug.is_empty() {
        return None;
    }
    for entry in mapping_table() {
        if entry.aliases.iter().any(|a| a.eq_ignore_ascii_case(&slug)) {
            return Some(entry.canonical.to_owned());
        }
    }
    None
}

/// Locale-aware display name for a canonical genre slug.
///
/// `display_locale` selects the language; falls back to English
/// when the table doesn't carry a localised string. Returns
/// `"Unknown"` for slugs the table doesn't know.
///
/// # Examples
///
/// ```
/// use ab_core::genre_code::display_name;
/// assert_eq!(display_name("science-fiction", "en"), "Science Fiction");
/// assert_eq!(display_name("science-fiction", "de"), "Science-Fiction");
/// assert_eq!(display_name("science-fiction", "es"), "Ciencia ficción");
/// assert_eq!(display_name("fantasy", "fr"), "Fantasy");
/// ```
#[must_use]
pub fn display_name(canonical: &str, display_locale: &str) -> &'static str {
    let locale_short = display_locale.split('-').next().unwrap_or(display_locale);
    for entry in mapping_table() {
        if !entry.canonical.eq_ignore_ascii_case(canonical) {
            continue;
        }
        for (loc, name) in entry.display_localized {
            if loc.eq_ignore_ascii_case(locale_short) {
                return name;
            }
        }
        return entry.display;
    }
    "Unknown"
}

/// Slugify: lowercase, non-alphanumeric → hyphen, collapse
/// runs, trim leading/trailing hyphens. ASCII-only — non-ASCII
/// chars in the input are stripped (use the localised name in
/// the table to match those).
///
/// Apostrophes are absorbed (not hyphen-replaced) so
/// "Children's Non-Fiction" produces "childrens-non-fiction"
/// rather than "children-s-non-fiction".
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_hyphen = true;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_hyphen = false;
        } else if c == '\'' || c == '\u{2019}' {
            // Apostrophe (straight + curly): word-internal,
            // doesn't break the run. Intentional no-op.
        } else if !last_hyphen {
            out.push('-');
            last_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

struct Entry {
    /// Canonical slug — what goes after `@` in tag form.
    canonical: &'static str,
    /// English display name (fallback).
    display: &'static str,
    /// Locale → display-name pairs.
    display_localized: &'static [(&'static str, &'static str)],
    /// Accepted slugified input forms.
    aliases: &'static [&'static str],
}

#[expect(
    clippy::too_many_lines,
    reason = "static genre mapping table; data, not logic"
)]
fn mapping_table() -> &'static [Entry] {
    static T: OnceLock<Vec<Entry>> = OnceLock::new();
    T.get_or_init(|| {
        vec![
            Entry {
                canonical: "fantasy",
                display: "Fantasy",
                display_localized: &[("de", "Fantasy"), ("fr", "Fantasy"), ("es", "Fantasía")],
                aliases: &["fantasy", "fantastik", "fantastique"],
            },
            Entry {
                canonical: "science-fiction",
                display: "Science Fiction",
                display_localized: &[
                    ("de", "Science-Fiction"),
                    ("fr", "Science-fiction"),
                    ("es", "Ciencia ficción"),
                ],
                aliases: &[
                    "science-fiction",
                    "sci-fi",
                    "scifi",
                    "sf",
                    "ciencia-ficcion",
                    "ciencia-ficción",
                ],
            },
            Entry {
                canonical: "mystery",
                display: "Mystery",
                display_localized: &[("de", "Krimi"), ("fr", "Mystère"), ("es", "Misterio")],
                aliases: &["mystery", "krimi", "mistery"],
            },
            Entry {
                canonical: "thriller",
                display: "Thriller",
                display_localized: &[("de", "Thriller"), ("fr", "Thriller"), ("es", "Thriller")],
                aliases: &["thriller"],
            },
            Entry {
                canonical: "horror",
                display: "Horror",
                display_localized: &[("de", "Horror"), ("fr", "Horreur"), ("es", "Terror")],
                aliases: &["horror", "horreur", "terror"],
            },
            Entry {
                canonical: "romance",
                display: "Romance",
                display_localized: &[("de", "Liebesroman"), ("fr", "Romance"), ("es", "Romance")],
                aliases: &["romance", "liebesroman"],
            },
            Entry {
                canonical: "crime",
                display: "Crime",
                display_localized: &[
                    ("de", "Kriminalroman"),
                    ("fr", "Policier"),
                    ("es", "Crimen"),
                ],
                aliases: &["crime", "kriminalroman", "policier", "crimen"],
            },
            Entry {
                canonical: "historical-fiction",
                display: "Historical Fiction",
                display_localized: &[
                    ("de", "Historischer Roman"),
                    ("fr", "Roman historique"),
                    ("es", "Ficción histórica"),
                ],
                aliases: &[
                    "historical-fiction",
                    "historical",
                    "historischer-roman",
                    "roman-historique",
                ],
            },
            Entry {
                canonical: "literary-fiction",
                display: "Literary Fiction",
                display_localized: &[
                    ("de", "Literarische Fiktion"),
                    ("fr", "Fiction littéraire"),
                    ("es", "Ficción literaria"),
                ],
                aliases: &["literary-fiction", "literary", "fiction"],
            },
            Entry {
                canonical: "young-adult",
                display: "Young Adult",
                display_localized: &[("de", "Jugendbuch"), ("fr", "Jeunesse"), ("es", "Juvenil")],
                aliases: &["young-adult", "ya", "jugendbuch", "jeunesse", "juvenil"],
            },
            Entry {
                canonical: "children",
                display: "Children",
                display_localized: &[("de", "Kinderbuch"), ("fr", "Enfance"), ("es", "Infantil")],
                aliases: &["children", "kids", "kinderbuch", "enfance", "infantil"],
            },
            Entry {
                canonical: "biography",
                display: "Biography",
                display_localized: &[
                    ("de", "Biografie"),
                    ("fr", "Biographie"),
                    ("es", "Biografía"),
                ],
                aliases: &["biography", "biografie", "biographie", "biografia"],
            },
            Entry {
                canonical: "memoir",
                display: "Memoir",
                display_localized: &[("de", "Memoiren"), ("fr", "Mémoires"), ("es", "Memorias")],
                aliases: &["memoir", "memoirs", "memoiren", "memoires", "memorias"],
            },
            Entry {
                canonical: "history",
                display: "History",
                display_localized: &[("de", "Geschichte"), ("fr", "Histoire"), ("es", "Historia")],
                aliases: &["history", "geschichte", "histoire", "historia"],
            },
            Entry {
                canonical: "self-help",
                display: "Self-Help",
                display_localized: &[
                    ("de", "Selbsthilfe"),
                    ("fr", "Développement personnel"),
                    ("es", "Autoayuda"),
                ],
                aliases: &[
                    "self-help",
                    "selfhelp",
                    "selbsthilfe",
                    "developpement-personnel",
                    "autoayuda",
                ],
            },
            Entry {
                canonical: "business",
                display: "Business",
                display_localized: &[("de", "Wirtschaft"), ("fr", "Économie"), ("es", "Empresa")],
                aliases: &["business", "wirtschaft", "economie", "empresa"],
            },
            Entry {
                canonical: "science",
                display: "Science",
                display_localized: &[("de", "Wissenschaft"), ("fr", "Science"), ("es", "Ciencia")],
                aliases: &["science", "wissenschaft", "ciencia"],
            },
            Entry {
                canonical: "philosophy",
                display: "Philosophy",
                display_localized: &[
                    ("de", "Philosophie"),
                    ("fr", "Philosophie"),
                    ("es", "Filosofía"),
                ],
                aliases: &["philosophy", "philosophie", "filosofia"],
            },
            Entry {
                canonical: "religion",
                display: "Religion",
                display_localized: &[("de", "Religion"), ("fr", "Religion"), ("es", "Religión")],
                aliases: &["religion", "religious"],
            },
            Entry {
                canonical: "poetry",
                display: "Poetry",
                display_localized: &[("de", "Lyrik"), ("fr", "Poésie"), ("es", "Poesía")],
                aliases: &["poetry", "lyrik", "poesie", "poesia"],
            },
            Entry {
                canonical: "drama",
                display: "Drama",
                display_localized: &[("de", "Drama"), ("fr", "Théâtre"), ("es", "Drama")],
                aliases: &["drama", "theatre", "teatro"],
            },
            Entry {
                canonical: "adventure",
                display: "Adventure",
                display_localized: &[("de", "Abenteuer"), ("fr", "Aventure"), ("es", "Aventura")],
                aliases: &["adventure", "abenteuer", "aventure", "aventura"],
            },
            Entry {
                canonical: "humor",
                display: "Humor",
                display_localized: &[("de", "Humor"), ("fr", "Humour"), ("es", "Humor")],
                aliases: &["humor", "humour", "comedy", "komedie"],
            },
            Entry {
                canonical: "travel",
                display: "Travel",
                display_localized: &[("de", "Reisen"), ("fr", "Voyage"), ("es", "Viajes")],
                aliases: &["travel", "reisen", "voyage", "viajes"],
            },
            Entry {
                canonical: "cooking",
                display: "Cooking",
                display_localized: &[("de", "Kochen"), ("fr", "Cuisine"), ("es", "Cocina")],
                aliases: &["cooking", "cookbook", "kochen", "cuisine", "cocina"],
            },
            Entry {
                canonical: "children-non-fiction",
                display: "Children's Non-Fiction",
                display_localized: &[
                    ("de", "Kindersachbuch"),
                    ("fr", "Documentaire jeunesse"),
                    ("es", "Infantil no ficción"),
                ],
                aliases: &[
                    "children-non-fiction",
                    "childrens-non-fiction",
                    "kids-nonfiction",
                ],
            },
            Entry {
                canonical: "non-fiction",
                display: "Non-Fiction",
                display_localized: &[
                    ("de", "Sachbuch"),
                    ("fr", "Documentaire"),
                    ("es", "No ficción"),
                ],
                aliases: &["non-fiction", "nonfiction", "sachbuch", "documentaire"],
            },
            Entry {
                canonical: "education",
                display: "Education",
                display_localized: &[("de", "Bildung"), ("fr", "Éducation"), ("es", "Educación")],
                aliases: &["education", "bildung", "educacion"],
            },
            Entry {
                canonical: "true-crime",
                display: "True Crime",
                display_localized: &[
                    ("de", "True Crime"),
                    ("fr", "True Crime"),
                    ("es", "Crimen real"),
                ],
                aliases: &["true-crime", "truecrime"],
            },
            Entry {
                canonical: "audiobook-classic",
                display: "Classic",
                display_localized: &[("de", "Klassiker"), ("fr", "Classique"), ("es", "Clásico")],
                aliases: &["classic", "classics", "klassiker", "classique", "clasico"],
            },
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_basic() {
        assert_eq!(
            normalize("Science Fiction").as_deref(),
            Some("science-fiction")
        );
        assert_eq!(normalize("Sci-Fi").as_deref(), Some("science-fiction"));
        assert_eq!(normalize("scifi").as_deref(), Some("science-fiction"));
        assert_eq!(normalize("SF").as_deref(), Some("science-fiction"));
    }

    #[test]
    fn normalize_case_and_whitespace() {
        assert_eq!(normalize("  FANTASY  ").as_deref(), Some("fantasy"));
        assert_eq!(normalize("\tNon-Fiction\n").as_deref(), Some("non-fiction"));
    }

    #[test]
    fn normalize_punctuation() {
        // "Children's Non-Fiction" → slug, then alias lookup
        assert_eq!(
            normalize("Children's Non-Fiction").as_deref(),
            Some("children-non-fiction"),
        );
        // En-dash, em-dash, etc. all collapse to hyphens.
        assert_eq!(normalize("Self—Help").as_deref(), Some("self-help"));
    }

    #[test]
    fn normalize_returns_none_for_unknown() {
        assert!(normalize("xxxx-unknown").is_none());
        assert!(normalize("").is_none());
        assert!(normalize("   ").is_none());
        assert!(normalize("!!!").is_none());
    }

    #[test]
    fn display_english() {
        assert_eq!(display_name("science-fiction", "en"), "Science Fiction");
        assert_eq!(display_name("fantasy", "en"), "Fantasy");
        assert_eq!(display_name("non-fiction", "en"), "Non-Fiction");
    }

    #[test]
    fn display_german() {
        assert_eq!(display_name("science-fiction", "de"), "Science-Fiction");
        assert_eq!(display_name("history", "de"), "Geschichte");
        assert_eq!(display_name("self-help", "de"), "Selbsthilfe");
    }

    #[test]
    fn display_french() {
        assert_eq!(display_name("mystery", "fr"), "Mystère");
        assert_eq!(display_name("travel", "fr"), "Voyage");
    }

    #[test]
    fn display_spanish() {
        assert_eq!(display_name("science-fiction", "es"), "Ciencia ficción");
        assert_eq!(display_name("history", "es"), "Historia");
    }

    #[test]
    fn display_unknown_canonical() {
        assert_eq!(display_name("xxx-unknown", "en"), "Unknown");
        assert_eq!(display_name("", "de"), "Unknown");
    }

    #[test]
    fn display_falls_back_to_english_for_missing_locale() {
        // Some entry probably has no "ja" — falls back to English.
        assert_eq!(display_name("fantasy", "ja"), "Fantasy");
    }

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Science Fiction"), "science-fiction");
        assert_eq!(slugify("Sci-Fi"), "sci-fi");
        assert_eq!(slugify("  HELLO  "), "hello");
        assert_eq!(slugify("a!b@c#d"), "a-b-c-d");
        assert_eq!(slugify(""), "");
    }
}
