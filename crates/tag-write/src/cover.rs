//! Cover-art HTTP fetcher (ADR-0028, slice C3a).
//!
//! `books.cover_url` is sourced from the catalog enrich path
//! (Audnexus / Audible CDNs). To embed the image into the file's
//! tag, we have to fetch the bytes first. This module owns that
//! HTTP path with:
//!
//! - A reusable `reqwest::Client` configured with the project's
//!   user agent + a per-request timeout from `HttpClientTunables`.
//! - A hard byte-budget cap (`cover_max_bytes`) enforced both at
//!   the `Content-Length` pre-check AND by counting bytes during
//!   the streaming read — a hostile CDN can omit / lie about
//!   `Content-Length`, so the runtime guard is the real defence.
//! - A small typed error enum so the stage's `run()` can branch
//!   on transient HTTP failures (log + skip cover, leave the
//!   `book_field_provenance` row in place for a retry) vs. fatal
//!   payload issues (the URL itself is wrong; logging that and
//!   carrying on is the only sensible action).
//!
//! The fetched bytes are returned raw; embedding via lofty's
//! `Picture::new_unchecked` happens in `crate::write`. The split
//! keeps this module sync-blocking-free (pure async / I/O) and
//! `write` sync-only (no runtime needed).

use std::time::Duration;

use ab_core::tunables::HttpClientTunables;
use reqwest::Client;

/// Typed cover-fetch failure surfaces.
#[derive(Debug, thiserror::Error)]
pub enum CoverFetchError {
    /// `reqwest::Client::build` failed at construction. Almost
    /// always a TLS-stack configuration bug rather than a
    /// per-request issue; surfaced as a fatal error.
    #[error("cover client build: {0}")]
    ClientBuild(String),
    /// The URL didn't parse or returned a non-success HTTP
    /// status. The book's `cover_url` is probably wrong (URL
    /// drift on the catalog side); log + skip is the operator
    /// flow.
    #[error("cover request: {0}")]
    Request(String),
    /// The remote claimed a payload larger than
    /// `cover_max_bytes` (Content-Length pre-check) OR streamed
    /// more than the cap during the read (runtime guard). The
    /// cover is dropped; tag-write proceeds with the other
    /// fields.
    #[error("cover payload too large (max {max_bytes} bytes)")]
    TooLarge {
        /// The configured cap.
        max_bytes: u64,
    },
    /// The fetched body was empty. An empty cover row is treated
    /// as no-op rather than embed-an-empty-picture.
    #[error("cover payload was empty")]
    Empty,
}

/// Reusable HTTP client for cover-art fetches.
#[derive(Clone, Debug)]
pub struct CoverClient {
    http: Client,
    max_bytes: u64,
}

impl CoverClient {
    /// Construct from the workspace's `HttpClientTunables`.
    ///
    /// # Errors
    ///
    /// Returns [`CoverFetchError::ClientBuild`] if `reqwest`'s
    /// TLS stack refuses to initialise. In practice this means
    /// a corrupt rustls cert store, which is fatal for the
    /// whole pipeline — surfaced so the stage's `run()` can
    /// log + skip rather than panic.
    pub fn new(tunables: &HttpClientTunables) -> Result<Self, CoverFetchError> {
        let ua = format!(
            "{}/{}",
            ab_core::build_info::APP_NAME,
            env!("CARGO_PKG_VERSION")
        );
        let http = Client::builder()
            .user_agent(ua)
            .timeout(Duration::from_secs(tunables.cover_fetch_timeout_secs))
            .build()
            .map_err(|e| CoverFetchError::ClientBuild(e.to_string()))?;
        Ok(Self {
            http,
            max_bytes: tunables.cover_max_bytes,
        })
    }

    /// Fetch the bytes at `url`. Streams the response, accumulating
    /// chunks into a `Vec<u8>` while enforcing the per-fetch byte
    /// cap. Returns the raw bytes on success — caller embeds via
    /// lofty.
    ///
    /// # Errors
    ///
    /// See [`CoverFetchError`].
    pub async fn fetch(&self, url: &str) -> Result<Vec<u8>, CoverFetchError> {
        use futures::StreamExt as _;
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| CoverFetchError::Request(format!("send: {e}")))?
            .error_for_status()
            .map_err(|e| CoverFetchError::Request(format!("status: {e}")))?;

        // `Content-Length` is advisory — some CDNs omit it on
        // chunked transfers. When present, reject up front to
        // save bandwidth; when absent, fall through to the
        // streaming guard below.
        if let Some(len) = response.content_length() {
            if len > self.max_bytes {
                return Err(CoverFetchError::TooLarge {
                    max_bytes: self.max_bytes,
                });
            }
        }

        let mut bytes: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| CoverFetchError::Request(format!("read chunk: {e}")))?;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "byte counts fit u64 trivially"
            )]
            let new_total = bytes.len() as u64 + chunk.len() as u64;
            if new_total > self.max_bytes {
                return Err(CoverFetchError::TooLarge {
                    max_bytes: self.max_bytes,
                });
            }
            bytes.extend_from_slice(&chunk);
        }

        if bytes.is_empty() {
            return Err(CoverFetchError::Empty);
        }
        Ok(bytes)
    }

    /// Effective byte cap, for diagnostics + error rendering.
    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Inject a pre-built client + byte cap directly. Used by
    /// the fallback in [`crate::stage::TagWriteEarlyStage::new`]
    /// when `reqwest::Client::builder` fails — we still want a
    /// usable struct so the daemon boots; downstream fetches
    /// surface their own errors.
    #[must_use]
    pub const fn with_parts(http: Client, max_bytes: u64) -> Self {
        Self { http, max_bytes }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::SocketAddr;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    /// Spin up a one-shot HTTP/1.1 server that serves the given
    /// bytes once with a fixed status + content-length. Returns
    /// the URL pointing at it. The task self-terminates after
    /// one accept.
    async fn one_shot_server(
        status_line: &'static str,
        body: Vec<u8>,
        include_content_length: bool,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let local: SocketAddr = listener.local_addr().expect("addr");
        let url = format!("http://{local}/cover.jpg");

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            // Read + discard the request headers up to the
            // double-CRLF.
            let mut buf = vec![0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;

            let mut response: Vec<u8> = Vec::new();
            write!(&mut response, "HTTP/1.1 {status_line}\r\n").unwrap();
            response.extend_from_slice(b"Content-Type: image/jpeg\r\n");
            if include_content_length {
                write!(&mut response, "Content-Length: {}\r\n", body.len()).unwrap();
            }
            response.extend_from_slice(b"Connection: close\r\n\r\n");
            response.extend_from_slice(&body);
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        });
        url
    }

    fn tiny_tunables(cap: u64) -> HttpClientTunables {
        HttpClientTunables {
            cover_max_bytes: cap,
            cover_fetch_timeout_secs: 5,
            ..HttpClientTunables::default()
        }
    }

    #[tokio::test]
    async fn fetches_a_small_jpeg_payload() {
        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        let url = one_shot_server("200 OK", body.clone(), true).await;
        let client = CoverClient::new(&tiny_tunables(1024)).expect("client");
        let got = client.fetch(&url).await.expect("fetch");
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn rejects_payload_over_content_length_cap() {
        // Server claims 4096-byte content; cap is 16.
        let body = vec![0u8; 4096];
        let url = one_shot_server("200 OK", body, true).await;
        let client = CoverClient::new(&tiny_tunables(16)).expect("client");
        let err = client.fetch(&url).await.expect_err("must Err");
        assert!(
            matches!(err, CoverFetchError::TooLarge { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_payload_when_streamed_over_cap_without_content_length() {
        // No Content-Length header → fall through to runtime
        // guard. 4 KB body, 16-byte cap.
        let body = vec![0xAA; 4096];
        let url = one_shot_server("200 OK", body, false).await;
        let client = CoverClient::new(&tiny_tunables(16)).expect("client");
        let err = client.fetch(&url).await.expect_err("must Err");
        assert!(
            matches!(err, CoverFetchError::TooLarge { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn surfaces_non_2xx_as_request_error() {
        let url = one_shot_server("404 Not Found", b"oops".to_vec(), true).await;
        let client = CoverClient::new(&tiny_tunables(1024)).expect("client");
        let err = client.fetch(&url).await.expect_err("must Err");
        assert!(matches!(err, CoverFetchError::Request(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn empty_body_surfaces_as_empty_error() {
        let url = one_shot_server("200 OK", Vec::new(), true).await;
        let client = CoverClient::new(&tiny_tunables(1024)).expect("client");
        let err = client.fetch(&url).await.expect_err("must Err");
        assert!(matches!(err, CoverFetchError::Empty), "got {err:?}");
    }
}
