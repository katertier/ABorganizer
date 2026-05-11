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
