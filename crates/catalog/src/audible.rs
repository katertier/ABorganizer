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
//! ## Regional hosts
//!
//! Audible publishes a per-country host with identical path /
//! response shape. The store the listing comes from controls
//! visibility: a UK-only audiobook returns 0 hits on
//! `api.audible.com` but lands on the front page of
//! `api.audible.co.uk`. `search-audible` walks regions in
//! `NetworkTunables.audible_region_order` (default 9-region
//! list mirroring the Audnexus walk from slice 2B) and stops on
//! the first non-empty response. The [`host_for_region`] helper
//! maps 2-letter region codes to fully-qualified hosts.
//!
//! Unknown region codes are skipped at the call site (logged at
//! `debug`); no fatal "unknown region" error so a typo in
//! `config.toml` doesn't take the search path down.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

use ab_core::Result;
use ab_core::tunables::HttpClientTunables;

/// Map a region code (`"us"`, `"uk"`, `"de"`, ...) to the
/// fully-qualified Audible catalog host. Returns `None` for
/// unknown codes — the caller's loop skips and tries the next.
///
/// Hosts pinned per Audible's documented public catalog
/// endpoints. The `.com.au` and `.co.uk` / `.co.jp` shapes are
/// deliberate; Audible's region naming isn't 1:1 with the TLD.
#[must_use]
pub const fn host_for_region(region: &str) -> Option<&'static str> {
    // `match` on byte content via `as_bytes` so the function
    // stays `const`. Each arm is the full origin (scheme + host)
    // since `format!` would heap-allocate.
    match region.as_bytes() {
        b"us" => Some("https://api.audible.com"),
        b"uk" => Some("https://api.audible.co.uk"),
        b"de" => Some("https://api.audible.de"),
        b"fr" => Some("https://api.audible.fr"),
        b"ca" => Some("https://api.audible.ca"),
        b"au" => Some("https://api.audible.com.au"),
        b"jp" => Some("https://api.audible.co.jp"),
        b"in" => Some("https://api.audible.in"),
        b"it" => Some("https://api.audible.it"),
        // Spain + Brazil added per ADR-0050 region-walk
        // completion (Libex confirms both resolve in production).
        b"es" => Some("https://api.audible.es"),
        b"br" => Some("https://api.audible.com.br"),
        _ => None,
    }
}

/// `response_groups` value for catalog search. Each group is a
/// dot-delimited bag of fields the API will include in each
/// product. The set picked here is the minimum for ASIN
/// disambiguation: title, author, runtime, language.
const SEARCH_RESPONSE_GROUPS: &str = "product_desc,product_attrs,contributors";

/// Hard cap on Android-screen author-bibliography pagination.
/// At ~50 books per page that's ≥ 500 books — well above any
/// real author catalog. Mirrors Libex's posture.
const ANDROID_AUTHOR_MAX_PAGES: u32 = 10;

/// User-Agent string mirroring the Audible iOS app (ADR-0050 § 5).
///
/// Audible's public-but-undocumented JSON API tolerates many UAs
/// but rate-limits unfamiliar ones harder; using the iOS app's
/// real UA gets the same anti-bot posture the mobile clients
/// see. The Libex project (MIT, see ADR-0050) publishes the same
/// string in production without legal pushback.
///
/// `CFNetwork/1240.0.4` corresponds to iOS 14.x — a well-aged
/// build that's still in regular rotation, so we don't look like
/// a one-off bot. If Audible enforces against this UA we revert
/// to the generic build-info UA and accept the friction.
pub const AUDIBLE_IOS_UA: &str = "Audible/671 CFNetwork/1240.0.4 Darwin/20.6.0";

/// `site_variant` value used by the Audible Android app.
///
/// Hits `/1.0/searchsuggestions` (ADR-0050 § 5). Wired in by the
/// searchsuggestions caller; defined here so the constant lives
/// next to the iOS UA.
pub const AUDIBLE_ANDROID_SITE_VARIANT: &str = "android-mshop";

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
        let http = Client::builder()
            .user_agent(AUDIBLE_IOS_UA)
            .timeout(Duration::from_secs(tunables.audible_timeout_secs))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http }
    }

    /// Search by title + optional author against one regional
    /// Audible host. Returns the catalog products in relevance
    /// order. Pass `""` for `author` to search by title only
    /// (Audible's relevance ranker handles that case).
    ///
    /// `region` is a 2-letter code (`"us"`, `"uk"`, ...) routed
    /// through [`host_for_region`]. Unknown codes return
    /// [`ab_core::Error::Network`] so the caller's region-walk
    /// loop can log + skip.
    ///
    /// # Errors
    ///
    /// Returns [`ab_core::Error::Network`] on transport failures,
    /// non-success status codes, or unknown region codes.
    pub async fn search(
        &self,
        region: &str,
        title: &str,
        author: &str,
    ) -> Result<Vec<AudibleProduct>> {
        let host = host_for_region(region).ok_or_else(|| {
            ab_core::Error::Network(format!("audible search: unknown region `{region}`"))
        })?;
        let mut req = self
            .http
            .get(format!("{host}/1.0/catalog/products"))
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
            .map_err(|e| ab_core::Error::Network(format!("audible search [{region}]: {e}")))?;
        if !resp.status().is_success() {
            return Err(ab_core::Error::Network(format!(
                "audible search [{region}]: HTTP {}",
                resp.status()
            )));
        }
        let body: SearchResponse = resp.json().await.map_err(|e| {
            ab_core::Error::Network(format!("audible search parse [{region}]: {e}"))
        })?;
        Ok(body.products)
    }

    /// List every book by an author. Paginates the catalog
    /// endpoint until either no more results come back or
    /// `max_pages` pages have been consumed.
    ///
    /// Calls Audible's `GET /1.0/catalog/products?author=…&page=N`
    /// with `num_results=50` (Audible's per-page cap). Each call
    /// returns up to 50 products; the loop stops when a page
    /// returns fewer than 50 results. `max_pages` is the safety
    /// limit (default 10 → up to 500 books, more than any real
    /// author's catalog).
    ///
    /// # Errors
    ///
    /// Surfaces a [`ab_core::Error::Network`] on the first page
    /// that fails. Already-collected pages are discarded — partial
    /// results would mask underlying failures.
    pub async fn list_books_by_author(
        &self,
        region: &str,
        author: &str,
        max_pages: u32,
    ) -> Result<Vec<AudibleProduct>> {
        let host = host_for_region(region).ok_or_else(|| {
            ab_core::Error::Network(format!("audible author: unknown region `{region}`"))
        })?;
        let mut all: Vec<AudibleProduct> = Vec::new();
        for page in 1..=max_pages {
            let page_str = page.to_string();
            let resp = self
                .http
                .get(format!("{host}/1.0/catalog/products"))
                .query(&[
                    ("author", author),
                    ("response_groups", SEARCH_RESPONSE_GROUPS),
                    ("num_results", "50"),
                    ("page", &page_str),
                    ("products_sort_by", "ReleaseDate"),
                ])
                .send()
                .await
                .map_err(|e| ab_core::Error::Network(format!("audible author p{page}: {e}")))?;
            if !resp.status().is_success() {
                return Err(ab_core::Error::Network(format!(
                    "audible author p{page}: HTTP {}",
                    resp.status()
                )));
            }
            let body: SearchResponse = resp.json().await.map_err(|e| {
                ab_core::Error::Network(format!("audible author parse p{page}: {e}"))
            })?;
            let got = body.products.len();
            all.extend(body.products);
            // Audible returns fewer than `num_results` on the last
            // page; that's our termination signal.
            if got < 50 {
                break;
            }
        }
        Ok(all)
    }

    /// Fetch every book ASIN by an author via Audible's Android
    /// app screen endpoint (ADR-0050 § 2).
    ///
    /// Calls
    /// `GET /1.0/screens/audible-android-author-detail/{asin}?tabId=titles&applicationType=Android_App&surface=Android&session_id=<uuid>&local_time=<iso8601>&response_groups=always-returned`
    /// and walks `pageSectionContinuationToken` until exhausted.
    /// 10-page hard cap mirrors Libex's posture (≥ 500 books for
    /// the typical 50-per-page response — more than any real
    /// author's catalog).
    ///
    /// `session_id` is a fresh `uuid::Uuid::new_v4().simple()`
    /// per call. The endpoint isn't authenticated; the
    /// `session_id` is screen-load telemetry, not security-
    /// relevant (per ADR-0050 § 5 threat-model note).
    ///
    /// # Errors
    ///
    /// [`ab_core::Error::Network`] on transport, unknown region,
    /// non-success status, JSON parse, or session-id generation
    /// failure. Returns partial results from prior pages on the
    /// first failing page is **not** the policy: we discard
    /// accumulated ASINs and surface the failure so the caller's
    /// retry doesn't blend a stale-and-fresh result.
    pub async fn fetch_author_books_via_screen(
        &self,
        region: &str,
        author_asin: &str,
    ) -> Result<Vec<String>> {
        let host = host_for_region(region).ok_or_else(|| {
            ab_core::Error::Network(format!("audible android author: unknown region `{region}`"))
        })?;

        let session_id = uuid::Uuid::new_v4().simple().to_string();
        let mut all: Vec<String> = Vec::new();
        let mut continuation: Option<String> = None;

        for page in 0..ANDROID_AUTHOR_MAX_PAGES {
            let now_iso = chrono::Utc::now().to_rfc3339();
            let mut query: Vec<(&str, &str)> = vec![
                ("tabId", "titles"),
                ("author_asin", author_asin),
                ("title_source", "all"),
                ("session_id", &session_id),
                ("applicationType", "Android_App"),
                ("local_time", now_iso.as_str()),
                ("response_groups", "always-returned"),
                ("surface", "Android"),
            ];
            if let Some(token) = &continuation {
                query.push(("pageSectionContinuationToken", token.as_str()));
            }

            let url = format!("{host}/1.0/screens/audible-android-author-detail/{author_asin}");
            let resp = self
                .http
                .get(&url)
                .query(&query)
                .send()
                .await
                .map_err(|e| {
                    ab_core::Error::Network(format!(
                        "audible android author p{page} [{region}/{author_asin}]: {e}"
                    ))
                })?;
            if !resp.status().is_success() {
                return Err(ab_core::Error::Network(format!(
                    "audible android author p{page} [{region}/{author_asin}]: HTTP {}",
                    resp.status()
                )));
            }
            let body: AndroidAuthorScreen = resp.json().await.map_err(|e| {
                ab_core::Error::Network(format!(
                    "audible android author parse p{page} [{region}/{author_asin}]: {e}"
                ))
            })?;

            // Walk sections: find the one with both rows + a
            // pagination token (per Libex's traversal). Other
            // sections (recommendations etc.) are skipped. The
            // `for` consumes by value so pagination moves out
            // without a clone.
            let mut page_continuation: Option<String> = None;
            for section in body.sections {
                let Some(model) = section.model else {
                    continue;
                };
                if model.rows.is_empty() || section.pagination.is_none() {
                    continue;
                }
                for item in model.rows {
                    if let Some(meta) = item.product_metadata {
                        if !meta.asin.is_empty() && !all.iter().any(|a| a == &meta.asin) {
                            all.push(meta.asin);
                        }
                    }
                }
                page_continuation = section.pagination;
                break;
            }

            match page_continuation {
                Some(next) => continuation = Some(next),
                None => break, // last page
            }
        }

        Ok(all)
    }

    /// Fetch the `chapter_info` subset of Audible's content-
    /// metadata response (ADR-0050 § 3).
    ///
    /// Calls `GET /1.0/content/{asin}/metadata` with
    /// `response_groups=chapter_info` against the given region's
    /// host. Returns `Ok(None)` when Audible delivers the
    /// envelope without the nested `chapter_info` block (common
    /// for unshipped chapter timings); the caller routes that to
    /// "fall back to acoustic detection only" per ADR-0024.
    ///
    /// # Errors
    ///
    /// [`ab_core::Error::Network`] on transport failure, unknown
    /// region, non-success status, or JSON parse failure.
    pub async fn fetch_chapter_info(
        &self,
        region: &str,
        asin: &str,
    ) -> Result<Option<crate::chapter_info::ChapterInfo>> {
        let host = host_for_region(region).ok_or_else(|| {
            ab_core::Error::Network(format!("audible chapter_info: unknown region `{region}`"))
        })?;
        let resp = self
            .http
            .get(format!("{host}/1.0/content/{asin}/metadata"))
            .query(&[("response_groups", "chapter_info")])
            .send()
            .await
            .map_err(|e| {
                ab_core::Error::Network(format!("audible chapter_info [{region}/{asin}]: {e}"))
            })?;
        if !resp.status().is_success() {
            return Err(ab_core::Error::Network(format!(
                "audible chapter_info [{region}/{asin}]: HTTP {}",
                resp.status()
            )));
        }
        let body = resp.text().await.map_err(|e| {
            ab_core::Error::Network(format!("audible chapter_info read [{region}/{asin}]: {e}"))
        })?;
        crate::chapter_info::parse_response(&body).map_err(|e| {
            ab_core::Error::Network(format!("audible chapter_info parse [{region}/{asin}]: {e}"))
        })
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

/// Subset of the Android author-detail screen response we walk
/// for ASIN extraction (ADR-0050 § 2). The full response is much
/// richer (recommendation sections, social proof, etc.); we
/// deserialize only the bits the traversal needs.
#[derive(Debug, Deserialize)]
struct AndroidAuthorScreen {
    #[serde(default)]
    sections: Vec<AndroidAuthorSection>,
}

#[derive(Debug, Deserialize)]
struct AndroidAuthorSection {
    #[serde(default)]
    model: Option<AndroidSectionModel>,
    /// Continuation token for the next page within this section.
    /// `None` on the last page of a paginated section, or on
    /// sections that aren't paginated at all (recommendations).
    #[serde(default)]
    pagination: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AndroidSectionModel {
    #[serde(default)]
    rows: Vec<AndroidAuthorRow>,
}

#[derive(Debug, Deserialize)]
struct AndroidAuthorRow {
    #[serde(default)]
    product_metadata: Option<AndroidProductMetadata>,
}

#[derive(Debug, Deserialize)]
struct AndroidProductMetadata {
    #[serde(default)]
    asin: String,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::host_for_region;

    #[test]
    fn host_for_region_maps_every_default_region() {
        // The default `audible_region_order` in `NetworkTunables`
        // is `[us, uk, de, fr, ca, au, jp, in, it, es, br]`. Every
        // code in that list must produce a known host, or the
        // region-walk skips them all silently.
        for code in [
            "us", "uk", "de", "fr", "ca", "au", "jp", "in", "it", "es", "br",
        ] {
            assert!(
                host_for_region(code).is_some(),
                "region `{code}` (default tunable) must map to a host",
            );
        }
    }

    #[test]
    fn host_for_region_returns_origin_not_full_url_path() {
        // Every value should be `https://api.audible.<tld>` with
        // no trailing slash and no path — callers append
        // `/1.0/catalog/products` themselves.
        for code in ["us", "uk", "de", "fr", "ca", "au", "jp", "in", "it"] {
            let host = host_for_region(code).expect("known code");
            assert!(host.starts_with("https://api.audible."), "{host}");
            assert!(!host.ends_with('/'), "no trailing slash on {host}");
            assert!(!host[8..].contains('/'), "no path component on {host}");
        }
    }

    #[test]
    fn host_for_region_returns_none_for_unknown_codes() {
        assert!(host_for_region("").is_none());
        assert!(host_for_region("zz").is_none());
        assert!(host_for_region("US").is_none(), "case-sensitive");
        assert!(host_for_region("usa").is_none(), "exact 2-letter only");
    }

    #[test]
    fn host_for_region_uk_is_co_uk_not_uk() {
        // Audible's UK host is `api.audible.co.uk` (not `.uk`).
        // Pin this so a future "simplify TLDs" refactor doesn't
        // silently break UK searches.
        assert_eq!(host_for_region("uk"), Some("https://api.audible.co.uk"));
        assert_eq!(host_for_region("au"), Some("https://api.audible.com.au"));
        assert_eq!(host_for_region("jp"), Some("https://api.audible.co.jp"));
    }

    /// Real-Audible integration test for `fetch_chapter_info`.
    ///
    /// Skipped in CI (no mock infra by operator direction —
    /// `#[ignore]` keeps the harness available locally without
    /// burning Audible API budget on every PR). Run explicitly:
    ///
    /// ```bash
    /// cargo test -p ab-catalog --test '*' -- --ignored
    /// # or
    /// cargo test -p ab-catalog audible::tests::fetch_chapter_info \
    ///     -- --ignored --nocapture
    /// ```
    ///
    /// ASIN `B017V4IM1G` (Brandon Sanderson, *The Way of Kings*)
    /// is chosen because it's a published US-region book that
    /// has shipped chapter timings (so `chapter_info` is present
    /// and `is_accurate` is `true`). If Audible ever drops the
    /// timings or the ASIN becomes unavailable, swap to another
    /// stable US-region book.
    #[tokio::test]
    #[ignore = "hits real api.audible.com — run explicitly via --ignored"]
    async fn fetch_chapter_info_live_returns_present_for_known_asin() {
        use super::AudibleClient;
        use ab_core::tunables::HttpClientTunables;

        let client = AudibleClient::new(&HttpClientTunables::default());
        let result = client
            .fetch_chapter_info("us", "B017V4IM1G")
            .await
            .expect("fetch ok");

        let ci = result.expect("chapter_info present on this ASIN");
        assert!(
            ci.runtime_ms.unwrap_or(0) > 0,
            "expected non-zero runtime, got {:?}",
            ci.runtime_ms
        );
        // Brand durations may be 0 on some books — only assert
        // the field is decodable, not its exact value. For
        // diagnostic output during `--nocapture` runs, the
        // assertion-failure path above produces all needed
        // context.
    }

    /// Unknown region returns `ab_core::Error::Network`, not a
    /// silent skip. No network call.
    #[tokio::test]
    async fn fetch_chapter_info_unknown_region_errors() {
        use super::AudibleClient;
        use ab_core::tunables::HttpClientTunables;

        let client = AudibleClient::new(&HttpClientTunables::default());
        let err = client
            .fetch_chapter_info("zz", "B000000000")
            .await
            .expect_err("unknown region must error");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown region"),
            "expected unknown-region message, got {msg}"
        );
    }

    /// Real-Audible integration test for
    /// `fetch_author_books_via_screen` (ADR-0050 § 2).
    ///
    /// Skipped in CI by `#[ignore]`. Run via:
    ///
    /// ```bash
    /// cargo test -p ab-catalog audible::tests::fetch_author_books_via_screen \
    ///     -- --ignored --nocapture
    /// ```
    ///
    /// Brandon Sanderson's author ASIN on the US store is
    /// `B000APZOQA`. He has 100+ titles, so a passing run
    /// exercises the continuation-token walk over multiple
    /// pages. If the ASIN ever rotates, swap to another
    /// stable high-output author.
    #[tokio::test]
    #[ignore = "hits real api.audible.com — run explicitly via --ignored"]
    async fn fetch_author_books_via_screen_live_returns_many_asins() {
        use super::AudibleClient;
        use ab_core::tunables::HttpClientTunables;

        let client = AudibleClient::new(&HttpClientTunables::default());
        let asins = client
            .fetch_author_books_via_screen("us", "B000APZOQA")
            .await
            .expect("fetch ok");
        assert!(
            asins.len() >= 20,
            "expected ≥ 20 ASINs for Brandon Sanderson, got {}",
            asins.len()
        );
        // ASIN shape: 10 alphanumeric chars per Audible convention.
        for asin in &asins {
            assert_eq!(asin.len(), 10, "ASIN `{asin}` should be exactly 10 chars");
        }
    }

    /// Unknown region returns `ab_core::Error::Network`, not a
    /// silent skip. No network call.
    #[tokio::test]
    async fn fetch_author_books_via_screen_unknown_region_errors() {
        use super::AudibleClient;
        use ab_core::tunables::HttpClientTunables;

        let client = AudibleClient::new(&HttpClientTunables::default());
        let err = client
            .fetch_author_books_via_screen("zz", "B000000000")
            .await
            .expect_err("unknown region must error");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown region"),
            "expected unknown-region message, got {msg}"
        );
    }
}
