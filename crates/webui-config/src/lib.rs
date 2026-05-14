//! Config web UI.
//!
//! Server-rendered via Askama templates + htmx for interactivity.
//! Bundled into the binary at compile time. No build-step dependency
//! (no Node, no Bun).

use askama::Template;
use axum::Router;
use axum::response::Html;
use axum::routing::get;

/// Mount the config UI under `/config`.
pub fn build_router() -> Router {
    Router::new().route("/", get(index))
}

// Askama generates a `render()` impl that reads every field; the
// Rust compiler can't see through the macro so we suppress dead_code.
#[derive(Template)]
#[template(path = "index.html")]
#[allow(dead_code)]
struct IndexPage<'a> {
    app_name: &'a str,
    display_name: &'a str,
    version: &'a str,
}

async fn index() -> Html<String> {
    let page = IndexPage {
        app_name: ab_core::build_info::APP_NAME,
        display_name: ab_core::build_info::DISPLAY_NAME,
        version: ab_core::build_info::VERSION,
    };
    Html(
        page.render()
            .unwrap_or_else(|e| format!("template error: {e}")),
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test assertions panic with explicit messages — the right shape for a smoke test"
)]
mod tests {
    //! Smoke test the only route. Catches the axum 0.7-vs-0.8
    //! placeholder-syntax bug class — the workspace-level
    //! `xtask route-tests` lint requires this coverage exist.

    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn root_returns_200() {
        let router = build_router();
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("request builder"),
            )
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
