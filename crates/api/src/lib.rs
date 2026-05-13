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
pub mod cleanup_targets;
pub mod error;
pub mod names;
pub mod reports;
pub mod router;
pub mod state;

pub use cleanup_targets::ExpiredPairingCodesTarget;
pub use error::ApiError;
pub use router::build_router;
pub use state::ApiState;
