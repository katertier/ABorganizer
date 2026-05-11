//! `aborg-daemon` — single-process daemon.
//!
//! Hosts (default features):
//!
//! * Native API at `/api/v1/` on the API port
//! * Player web UI at `/player/`
//! * Config web UI at `/config/`
//! * `Audiobookshelf`-compat API at root, on the ABS port
//! * Background pipeline workers (scan, fingerprint, enrich, ...)
//!
//! Cargo features (`tagger`, `player`, `shelf`) gate which routers
//! are registered. Default builds enable all three.

#![allow(
    missing_docs,
    // Internal modules use pub(crate) — clippy flags this as redundant
    // inside a binary crate, but it's the canonical visibility.
    clippy::redundant_pub_crate
)]

mod lockfile;
mod logging;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::Router;
use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use ab_core::{Tunables, paths};
use ab_db::{EphemeralDb, LibraryDb};

#[derive(Debug, Parser)]
#[command(
    name = ab_core::build_info::DAEMON_NAME,
    version = ab_core::build_info::VERSION,
    about = "ABorganizer daemon",
)]
struct Args {
    /// Run in foreground (default when no `LaunchAgent` is involved).
    #[arg(long)]
    foreground: bool,

    /// Path to config TOML (default: `<storage_root>/config.toml`).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init();

    info!(
        app = ab_core::build_info::APP_NAME,
        version = ab_core::build_info::VERSION,
        foreground = args.foreground,
        "daemon.start"
    );

    let storage_root = paths::app_support_dir();
    std::fs::create_dir_all(&storage_root).context("create storage root")?;

    // Acquire exclusive lock. Kernel releases on process exit; no
    // stale-detection logic needed.
    let _lock =
        lockfile::acquire(&storage_root.join("daemon.lock")).context("acquire daemon lockfile")?;

    let tunables = Tunables::default();

    // Open both databases. Pool sizing + busy-timeout come from
    // `tunables.db` (single source of truth in `ab_core::tunables`).
    let library_path = storage_root.join("library.db");
    let ephemeral_path = storage_root.join("ephemeral.db");
    let library = LibraryDb::open(&library_path, &tunables.db)
        .await
        .context("open library db")?;
    let ephemeral = EphemeralDb::open(&ephemeral_path, &tunables.db)
        .await
        .context("open ephemeral db")?;

    // Shared state for the API router.
    let api_state = ab_api::ApiState::new(library.clone(), ephemeral.clone());

    // Cancellation propagated to every worker on SIGTERM.
    let cancel = CancellationToken::new();
    spawn_signal_handlers(&cancel);

    // Build the unified Router for the API port (api + webuis).
    let mut router = Router::new()
        .nest("/api/v1", ab_api::build_router(api_state))
        .nest("/config", ab_webui_config::build_router());
    #[cfg(feature = "player")]
    {
        router = router.nest("/player", ab_webui_player::build_router());
    }

    let api_addr: SocketAddr = format!("{}:{}", tunables.server.bind, tunables.server.api_port)
        .parse()
        .context("parse API bind address")?;

    info!(addr = %api_addr, "daemon.api.bind");

    let api_listener = tokio::net::TcpListener::bind(api_addr)
        .await
        .context("bind API listener")?;

    let api_serve = axum::serve(api_listener, router.into_make_service())
        .with_graceful_shutdown(cancel.clone().cancelled_owned());

    // Optionally serve the ABS-compat API on its own port.
    #[cfg(feature = "shelf")]
    let abs_serve = if tunables.server.abs_enabled {
        let abs_addr: SocketAddr = format!("{}:{}", tunables.server.bind, tunables.server.abs_port)
            .parse()
            .context("parse ABS bind address")?;
        info!(addr = %abs_addr, "daemon.abs.bind");
        let abs_listener = tokio::net::TcpListener::bind(abs_addr)
            .await
            .context("bind ABS listener")?;
        Some(
            axum::serve(abs_listener, ab_shelf::build_router().into_make_service())
                .with_graceful_shutdown(cancel.clone().cancelled_owned()),
        )
    } else {
        None
    };

    // Run servers concurrently and wait for shutdown.
    #[cfg(feature = "shelf")]
    let result = if let Some(abs_serve) = abs_serve {
        tokio::try_join!(api_serve.into_future(), abs_serve.into_future()).map(|_| ())
    } else {
        api_serve.await
    };

    #[cfg(not(feature = "shelf"))]
    let result = api_serve.await;

    if let Err(e) = result {
        warn!(error = %e, "daemon.serve.error");
    }

    info!("daemon.stop");
    Ok(())
}

fn spawn_signal_handlers(cancel: &CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal as unix_signal};

    let c = cancel.clone();
    tokio::spawn(async move {
        if let Ok(mut sig) = unix_signal(SignalKind::terminate()) {
            sig.recv().await;
            info!("daemon.signal.sigterm");
            c.cancel();
        }
    });
    let c = cancel.clone();
    tokio::spawn(async move {
        if signal::ctrl_c().await.is_ok() {
            info!("daemon.signal.sigint");
            c.cancel();
        }
    });
}
