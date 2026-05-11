//! Top-level error type used at module / crate boundaries.
//!
//! Per `docs/POLICIES.md`: `thiserror` for typed errors at library
//! boundaries; `anyhow` is reserved for application-level wiring inside
//! `bins/`. Crates never expose `anyhow::Error` in public APIs.

use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Application-wide typed error.
///
/// New variants are added with intent — each represents a recovery
/// boundary in the pipeline or API. Wrappers around `std::io::Error`
/// or third-party error types use the `#[from]` attribute so the
/// `?` operator just works.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// I/O failure. Wraps any underlying `std::io::Error`.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration is malformed or references nonexistent resources.
    #[error("config: {0}")]
    Config(String),

    /// A pipeline stage failed in a way the executor can't classify.
    #[error("pipeline stage {stage}: {message}")]
    Stage {
        /// Stage name that failed.
        stage: &'static str,
        /// Human-readable message.
        message: String,
    },

    /// A downstream HTTP service returned an error.
    #[error("network: {0}")]
    Network(String),

    /// Database error. SQLite / sqlx propagation point.
    #[error("database: {0}")]
    Database(String),

    /// A user-supplied path falls outside the allowed roots.
    #[error("path {0:?} is outside the allowed library roots")]
    PathOutsideAllowed(std::path::PathBuf),

    /// A precondition or invariant was violated.
    #[error("invariant: {0}")]
    Invariant(&'static str),
}

impl Error {
    /// Construct a [`Self::Stage`] error from a stage name and message.
    pub fn stage(stage: &'static str, message: impl Into<String>) -> Self {
        Self::Stage {
            stage,
            message: message.into(),
        }
    }
}
