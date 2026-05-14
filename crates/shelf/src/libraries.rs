//! `GET /api/libraries` — list libraries.
//!
//! ABorganizer's model is **one library per daemon** — there's
//! no multi-library partitioning (every book lives under the
//! `library_roots` set, but they're presented as a single
//! virtual library to ABS clients). This handler returns a
//! single fixed-id library named after `tunables.library_name`
//! (planned; for now we hardcode "Audiobooks").
//!
//! The ABS shape is `{ libraries: [{ id, name, ... }] }` —
//! ABS clients expect the array under that key. The
//! `media_type` field (`"book"` for audiobook libraries) is
//! the one downstream clients use to choose their layout.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::error::ShelfError;
use crate::state::ShelfState;

/// Fixed library identity for the single-library model. ABS
/// clients persist this — keep it stable across releases.
pub const LIBRARY_ID: &str = "aborg-default";

/// Library entry as serialised in the `/api/libraries` response.
///
/// Field names match ABS's expected JSON keys (`snake_case` →
/// `camelCase` not applied; ABS uses snake-case for `media_type`
/// and `display_order` historically, plus straight camelCase
/// for newer fields — we emit a conservative subset).
#[derive(Debug, Clone, Serialize)]
pub struct Library {
    /// Stable identifier ABS clients use to scope subsequent
    /// requests.
    pub id: String,
    /// Operator-visible name. Hardcoded for now; future slice
    /// reads from a tunable.
    pub name: String,
    /// `"book"` is the audiobook-library marker — the field
    /// ABS clients key off to render audiobook UX.
    #[serde(rename = "mediaType")]
    pub media_type: &'static str,
}

/// Top-level response shape — ABS expects the array under a
/// `libraries` key.
#[derive(Debug, Serialize)]
pub struct LibrariesResponse {
    pub libraries: Vec<Library>,
}

/// `GET /api/libraries`.
///
/// # Errors
///
/// Currently infallible (single hardcoded library); the
/// `Result` shape stays so the future tunable-backed variant
/// can plug in without breaking the signature.
#[allow(clippy::unused_async, reason = "axum handler signature parity")]
pub async fn list_libraries(
    State(_state): State<ShelfState>,
) -> Result<Json<LibrariesResponse>, ShelfError> {
    Ok(Json(LibrariesResponse {
        libraries: vec![Library {
            id: LIBRARY_ID.to_owned(),
            name: "Audiobooks".to_owned(),
            media_type: "book",
        }],
    }))
}
