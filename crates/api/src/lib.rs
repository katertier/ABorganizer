//! Native HTTP API.
//!
//! All endpoints are versioned under `/api/v1/`. Companion crates
//! (`ab-shelf`, `ab-webui-config`, `ab-webui-player`) mount their own
//! routers at separate paths.
//!
//! # Auth
//!
//! Bearer token in `Authorization: Bearer <hex>`. Tokens are issued
//! via the pairing flow (see `docs/SECURITY.md`). Localhost requests
//! without a token are permitted on the loopback interface only when
//! `[server] localhost_passthrough = true` (default true).
//!
//! # Errors
//!
//! Handlers return `Result<T, ApiError>` where `ApiError` implements
//! `IntoResponse`. JSON error bodies follow RFC 7807 (Problem Details).

#![allow(missing_docs)] // scaffold

pub mod audiologo_apply;
pub mod audiologo_review;
pub mod auth;
pub mod authors;
pub mod background;
pub mod books_playlist;
pub mod books_transcript;
pub mod cleanup_targets;
pub mod doctor;
pub mod error;
pub mod library_roots;
pub mod names;
pub mod narrators;
pub mod pagination;
pub mod pairing;
pub mod progress;
pub mod rate_limit;
pub mod reports;
pub mod router;
pub mod saved_queries;
pub mod search;
pub mod series;
pub mod state;
pub mod stats;
pub mod tokens;
pub mod user_edits;

pub use cleanup_targets::{ExpiredPairingCodesTarget, StaleCompanionHintsTarget};
pub use error::ApiError;
pub use router::build_router;
pub use state::ApiState;
