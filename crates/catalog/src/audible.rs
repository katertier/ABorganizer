//! Audible catalog client.
//!
//! Audible publishes a public-but-undocumented JSON API at
//! `api.audible.com` that the official mobile apps use. We hit it
//! directly with `reqwest` — no HTML parsing required.
//!
//! Endpoints used:
//!
//! * `GET /1.0/catalog/products?title=…&author=…&response_groups=…`
//!   — keyword search, returns up to N candidate products.
//! * `GET /1.0/catalog/products/{asin}?response_groups=…`
//!   — product detail for a known ASIN.
//!
//! Region routing (`.com`, `.de`, `.co.uk`, …) lives in a later
//! slice; for now the US host handles every search.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

use ab_core::Result;
use ab_core::tunables::HttpClientTunables;

/// Host used for catalog search. Regional hosts (`api.audible.de`
/// etc.) follow the same path/response shape, but we stick to the
/// US endpoint until a "region walk on Audible miss" slice lands.
const BASE: &str = "https://api.audible.com";

/// `response_groups` value for catalog search. Each group is a
/// dot-delimited bag of fields the API will include in each
/// product. The set picked here is the minimum for ASIN
/// disambiguation: title, author, runtime, language.
const SEARCH_RESPONSE_GROUPS: &str = "product_desc,product_attrs,contributors";

/// Reusable Audible HTTP client. Carries one `reqwest::Client`.
#[derive(Clone)]
pub struct AudibleClient {
    http: Client,
}

impl AudibleClient {
    /// Construct with our user agent. Timeout from
    /// `tunables.audible_timeout_secs`.
    #[must_use]
    pub fn new(tunables: &HttpClientTunables) -> Self {
        let ua = format!(
            "{}/{}",
            ab_core::build_info::APP_NAME,
            env!("CARGO_PKG_VERSION")
        );
        let http = Client::builder()
            .user_agent(ua)
            .timeout(Duration::from_secs(tunables.audible_timeout_secs))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http }
    }

    /// Search by title + optional author. Returns the catalog
    /// products in relevance order. Pass `""` for `author` to
    /// search by title only (which Audible's relevance ranker
    /// handles reasonably).
    ///
    /// # Errors
    ///
    /// Returns [`ab_core::Error::Network`] on transport failures
    /// or non-success status codes.
    pub async fn search(&self, title: &str, author: &str) -> Result<Vec<AudibleProduct>> {
        let mut req = self
            .http
            .get(format!("{BASE}/1.0/catalog/products"))
            .query(&[
                ("title", title),
                ("response_groups", SEARCH_RESPONSE_GROUPS),
                ("num_results", "10"),
                ("products_sort_by", "Relevance"),
            ]);
        if !author.is_empty() {
            req = req.query(&[("author", author)]);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| ab_core::Error::Network(format!("audible search: {e}")))?;
        if !resp.status().is_success() {
            return Err(ab_core::Error::Network(format!(
                "audible search: HTTP {}",
                resp.status()
            )));
        }
        let body: SearchResponse = resp
            .json()
            .await
            .map_err(|e| ab_core::Error::Network(format!("audible search parse: {e}")))?;
        Ok(body.products)
    }

    /// Underlying HTTP client.
    #[must_use]
    pub const fn http(&self) -> &Client {
        &self.http
    }
}

impl Default for AudibleClient {
    fn default() -> Self {
        Self::new(&HttpClientTunables::default())
    }
}

/// One Audible catalog product. Only the fields we currently
/// disambiguate against are deserialized; extend as needed.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // scaffold; downstream fields used in later slices
pub struct AudibleProduct {
    pub asin: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default, rename = "runtime_length_min")]
    pub runtime_length_min: Option<u32>,
    #[serde(default)]
    pub authors: Vec<AudibleContributor>,
    #[serde(default)]
    pub narrators: Vec<AudibleContributor>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default, rename = "release_date")]
    pub release_date: Option<String>,
}

/// Author / narrator entry on an Audible product.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct AudibleContributor {
    pub name: String,
    #[serde(default)]
    pub asin: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    products: Vec<AudibleProduct>,
}
