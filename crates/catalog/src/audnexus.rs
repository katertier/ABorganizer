//! Audnexus HTTP client.
//!
//! Endpoint catalogue (verified at scaffolding time):
//!
//! * `GET /authors/{asin}` → author metadata
//! * `GET /books/{asin}`   → book metadata
//! * `GET /books/{asin}/chapters` → chapter list + brand intro/outro durations
//!
//! Region is encoded as a subdomain (`api.audnex.us` is the
//! aggregate; `<region>.audnex.us` exists historically — current
//! single-host pattern at `api.audnex.us/{region}/...`).

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

use ab_core::tunables::HttpClientTunables;
use ab_core::{Error, Result};

/// Reusable HTTP client. Carries a single `reqwest::Client`.
#[derive(Clone)]
pub struct AudnexusClient {
    http: Client,
    user_agent: String,
}

impl AudnexusClient {
    /// Construct with a user agent identifying our app per Audnexus
    /// project request (honest-identification policy). Timeout from
    /// `tunables.audnexus_timeout_secs`.
    pub fn new(tunables: &HttpClientTunables) -> Self {
        let ua = format!(
            "{}/{} (+{})",
            ab_core::build_info::APP_NAME,
            env!("CARGO_PKG_VERSION"),
            ab_core::build_info::HOMEPAGE_URL
        );
        let http = Client::builder()
            .user_agent(ua.clone())
            .timeout(Duration::from_secs(tunables.audnexus_timeout_secs))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            http,
            user_agent: ua,
        }
    }

    /// Look up a book by ASIN within a single region.
    ///
    /// # Errors
    ///
    /// Returns [`ab_core::Error::Network`] on transport failures or
    /// non-success status codes; [`ab_core::Error::Database`] is never
    /// raised by this method.
    pub async fn lookup_book(&self, region: &str, asin: &str) -> Result<Option<AudnexusBook>> {
        let url = format!("https://api.audnex.us/books/{asin}?region={region}");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Network(format!("audnexus get: {e}")))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(Error::Network(format!(
                "audnexus {}: HTTP {}",
                asin,
                resp.status()
            )));
        }
        let book = resp
            .json::<AudnexusBook>()
            .await
            .map_err(|e| Error::Network(format!("audnexus parse: {e}")))?;
        Ok(Some(book))
    }

    /// Look up chapter data for a book by ASIN within a single
    /// region. Returns `None` when the book exists but has no
    /// chapter `ToC` (which Audnexus expresses as a 404 on this
    /// endpoint specifically).
    ///
    /// # Errors
    ///
    /// Returns [`ab_core::Error::Network`] on transport failures or
    /// non-success status codes (other than 404).
    pub async fn lookup_chapters(
        &self,
        region: &str,
        asin: &str,
    ) -> Result<Option<AudnexusChapters>> {
        let url = format!("https://api.audnex.us/books/{asin}/chapters?region={region}");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Network(format!("audnexus chapters get: {e}")))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(Error::Network(format!(
                "audnexus chapters {}: HTTP {}",
                asin,
                resp.status()
            )));
        }
        let chapters = resp
            .json::<AudnexusChapters>()
            .await
            .map_err(|e| Error::Network(format!("audnexus chapters parse: {e}")))?;
        Ok(Some(chapters))
    }

    /// User agent string in use.
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }
}

impl Default for AudnexusClient {
    fn default() -> Self {
        Self::new(&HttpClientTunables::default())
    }
}

/// Subset of the Audnexus book response we depend on. Extend in
/// follow-up work as fields become needed.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // scaffold
pub struct AudnexusBook {
    pub asin: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default, rename = "publisherName")]
    pub publisher_name: Option<String>,
    #[serde(default, rename = "releaseDate")]
    pub release_date: Option<String>,
    #[serde(default, rename = "runtimeLengthMin")]
    pub runtime_length_min: Option<u32>,
    /// Authors are an ordered array; primary author first.
    #[serde(default)]
    pub authors: Vec<AudnexusContributor>,
    /// Narrators are an ordered array; can be 1..N people.
    #[serde(default)]
    pub narrators: Vec<AudnexusContributor>,
    /// Genres + sub-categories Audnexus assigns to the book.
    /// Includes both top-level genres ("Fantasy") and the
    /// sub-genre tags Audible calls "tags". The `type` field
    /// differentiates them; we currently treat both as genre
    /// candidates (the consensus stage can refine later).
    #[serde(default)]
    pub genres: Vec<AudnexusGenre>,
}

/// One genre entry on an Audnexus book response.
///
/// Audnexus assigns its own ASINs to genres + sub-genres
/// (`/genres/{asin}` endpoint). The ASIN goes into
/// `genres.audible_id`; the name goes through
/// [`ab_core::genre_code::normalize`] to produce the canonical
/// slug stored in `book_field_provenance.value`.
#[derive(Debug, Clone, Deserialize)]
pub struct AudnexusGenre {
    pub name: String,
    #[serde(default)]
    pub asin: Option<String>,
    /// `"Genres"` for top-level, `"Tags"` for sub-genre tags.
    /// Retained for future filtering; not currently used.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// One author or narrator entry on an Audnexus book response.
///
/// Audnexus assigns its own ASINs to people (visible in their
/// `/authors/{asin}` endpoint) — kept here so the identity-resolve
/// stage can use them as the join key when available.
#[derive(Debug, Clone, Deserialize)]
pub struct AudnexusContributor {
    pub name: String,
    #[serde(default)]
    pub asin: Option<String>,
}

/// `/books/{asin}/chapters` response.
///
/// Audnexus computes chapter boundaries from Audible's master `ToC`
/// and includes durations for the brand intro / outro (publisher
/// jingle) when the book has them. Books distributed by Audible
/// Studios + other major imprints almost always carry brand
/// markers; smaller indies skip them.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct AudnexusChapters {
    pub asin: String,
    /// Brand intro duration in milliseconds. Zero when no intro
    /// marker is present.
    #[serde(default, rename = "brandIntroDurationMs")]
    pub brand_intro_duration_ms: u64,
    /// Brand outro duration in milliseconds. Zero when none.
    #[serde(default, rename = "brandOutroDurationMs")]
    pub brand_outro_duration_ms: u64,
    /// True when Audnexus has high-confidence chapter timings
    /// (verified against the Audible-supplied `ToC`).
    #[serde(default, rename = "isAccurate")]
    pub is_accurate: bool,
    /// Ordered chapter list. Empty when Audible didn't supply a
    /// `ToC` (rare for full-length audiobooks, common for samples).
    #[serde(default)]
    pub chapters: Vec<AudnexusChapter>,
}

/// One chapter on an Audnexus chapters response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct AudnexusChapter {
    #[serde(default, rename = "lengthMs")]
    pub length_ms: u64,
    #[serde(default, rename = "startOffsetMs")]
    pub start_offset_ms: u64,
    #[serde(default)]
    pub title: String,
}
