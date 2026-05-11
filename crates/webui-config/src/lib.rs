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
