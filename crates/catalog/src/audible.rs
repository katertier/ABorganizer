//! Audible product page scraper.
//!
//! Audible has no public REST API for catalog data. We scrape the
//! product pages, parsing the structured `<script type="application/ld+json">`
//! blocks (schema.org/AudiobookDigitalDocument) which carry stable
//! title/author/duration/publisher data across all region domains.
//!
//! Rate-limit policy: 120 ms pacing between requests per region.

use std::time::Duration;

use reqwest::Client;

use ab_core::Result;
use ab_core::tunables::HttpClientTunables;

/// Reusable Audible HTTP client.
#[derive(Clone)]
pub struct AudibleClient {
    http: Client,
}

impl AudibleClient {
    /// Construct with our user agent. Timeout from
    /// `tunables.audible_timeout_secs`.
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

    /// Search by title + author. Returns up to N candidate ASINs.
    ///
    /// # Errors
    ///
    /// Returns [`ab_core::Error::Network`] on transport failures.
    #[allow(clippy::unused_async)] // stub; async signature reserved for real impl
    pub async fn search(&self, _title: &str, _author: &str, _region: &str) -> Result<Vec<String>> {
        // TODO: implement once the scraper is ported from the previous codebase.
        Ok(Vec::new())
    }

    /// Underlying HTTP client.
    pub const fn http(&self) -> &Client {
        &self.http
    }
}

impl Default for AudibleClient {
    fn default() -> Self {
        Self::new(&HttpClientTunables::default())
    }
}
