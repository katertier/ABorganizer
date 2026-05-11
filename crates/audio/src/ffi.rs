//! Stub for the Swift FFI surface.
//!
//! The real implementation is gated on a successful Swift build
//! (`build.rs` will detect SDK availability and emit
//! `--cfg=ab_audio_bridge`). Until that lands, this module exposes
//! `is_bridge_compiled()` returning `false` so callers can degrade
//! gracefully.

/// True when the Swift bridge is linked into this binary.
pub const fn is_bridge_compiled() -> bool {
    cfg!(ab_audio_bridge)
}
