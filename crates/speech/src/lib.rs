//! Apple Speech + Natural Language framework bridge.
//!
//! Thin Rust wrapper over the Swift FFI in `swift/aborg_speech.swift`,
//! compiled to a static lib by `build.rs` and linked in at build time.
//!
//! Two surfaces:
//!
//! 1. [`bridge`] — Speech framework calls: `transcribe_window`,
//!    `install_speech_model`, `speech_locale_status`. Async; one
//!    Swift `Task` per call; typed errors via [`BridgeError`].
//! 2. [`language`] — `NLLanguageRecognizer` wrapper. Pure
//!    detection over text; no audio. Used pre- and post-
//!    transcribe to nail down the BCP-47 locale.
//!
//! No `ab-db` / `ab-pipeline` deps — callers (typically
//! `ab-transcript` stages) compose the bridge surface with their
//! own DB writes. That separation is the point of this crate:
//! a CLI tool or test harness can use the bridge without
//! dragging the pipeline machinery along.
//!
//! The bridge degrades to [`BridgeError::BridgeUnavailable`]
//! when:
//!
//! * compiled on a non-macOS target,
//! * built on macOS with no `swiftc` on PATH,
//! * the Swift source is missing.
//!
//! `build.rs` sets `cfg(aborg_speech_bridge)` on success;
//! otherwise the `extern "C"` block is omitted and the safe
//! wrappers return `BridgeUnavailable` synchronously.

pub mod bridge;
pub mod language;

pub use bridge::{
    BridgeError, LocaleStatusReport, TranscriptSegment, install_speech_model,
    install_speech_model_typed, speech_locale_status, transcribe_window, transcribe_window_typed,
};
pub use language::{
    LanguageDetection, LanguageHit, detect, detect as detect_language, detect_from_transcript,
};
