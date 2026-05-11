//! Logging init. OSLog always on (via the `tracing` subscriber's
//! formatting layer); file logs gated by `[log] file_enabled` in
//! the config (off by default).

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialise tracing. Honours `RUST_LOG` if set; otherwise defaults
/// to `info`.
pub(crate) fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,aborg=info,ab_pipeline=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(std::io::stderr),
        )
        .init();
}
