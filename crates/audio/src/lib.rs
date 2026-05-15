//! Audio I/O + Apple framework bridge.
//!
//! # Layers
//!
//! 1. **Pure-Rust probe** via [`lofty`]. Today: duration probe in
//!    [`info::probe_duration_ms`]. Tag-read lives in `ab-read-tags`
//!    (separate crate); PCM decode for fingerprinting lives in
//!    `ab-fingerprint` (separate crate, uses Symphonia). This
//!    crate intentionally stays small — just the bits the FFI
//!    layer's callers need to know about a file *before* dropping
//!    into Swift.
//!
//! 2. **Swift FFI bridge** for everything that needs Apple's
//!    frameworks: AVFoundation (transcode, encode, AVPlayer/AirPlay),
//!    Speech (transcription via `SpeechAnalyzer`), `NaturalLanguage`
//!    (`NLLanguageRecognizer`), `FoundationModels` (Apple Intelligence
//!    tag generation).
//!
//! The FFI surface uses the well-tested pattern from the previous
//! codebase: `@_cdecl` functions on the Swift side; `extern "C"`
//! declarations on the Rust side; oneshot channels carry results
//! out of fire-and-forget callbacks.
//!
//! # Safety
//!
//! `unsafe_code = "deny"` at the workspace level is lifted *only*
//! at FFI call sites with `#[expect(unsafe_code, reason = "…")]`
//! and a `// SAFETY: …` comment explaining the invariant being
//! upheld.

pub mod aax;
pub mod ffi;
pub mod info;

pub use aax::{AAX_CODEC_TAG, AaxInfo, read_info as read_aax_info};
pub use ffi::{
    BridgeError, is_bridge_compiled, read_samples_window, read_samples_window_typed,
    transcode_to_m4b, transcode_to_m4b_typed,
};
pub use info::probe_duration_ms;
