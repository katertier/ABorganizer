//! Bench: serve the config web UI standalone with no daemon backing.
//! Used for UI iteration without rebuilding the whole daemon.

use anyhow::Result;
use axum::Router;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let app = Router::new().nest("/config", ab_webui_config::build_router());

    let addr = "127.0.0.1:8470";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr, "webui_dev.bind");
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}
