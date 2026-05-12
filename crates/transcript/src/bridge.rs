//! Safe Rust wrapper around the `aborg_transcribe_window` Swift
//! FFI entry point.
//!
//! The Swift side runs `SpeechAnalyzer` in slice 3A.3; in 3A.2
//! it's a stub that returns a sentinel segment. Either way the
//! C ABI is identical, so the Rust safe wrapper here is the
//! permanent surface — callers don't care whether the body is
//! stubbed or real.
//!
//! # Contract
//!
//! On a successful call the Swift side fires the registered
//! callback exactly once with a JSON-encoded `[Segment]`. The
//! callback's `ctx` argument is an opaque `Box<oneshot::Sender>`
//! that this module leaks into Swift and recovers in the
//! callback. The `data_ptr` may be null on the Swift side to
//! signal failure; the wrapper translates that into
//! `Error::Stage`.
//!
//! # Non-macOS / no-swiftc fallback
//!
//! When `cfg(aborg_ai_bridge)` isn't set (non-macOS target or
//! swiftc missing at build time), the wrapper returns an
//! `Unavailable`-flavoured error at runtime so the crate still
//! compiles on Linux CI and the upstream caller can degrade
//! gracefully (skip the stage, log, move on).

use std::path::Path;

use ab_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// Typed Swift-FFI error variants. Mirrors the `kErrCode*`
/// constants in `swift/aborg_ai.swift`. Values are stable across
/// versions — new variants append, never reorder.
///
/// Callers that care about a specific failure (e.g. the
/// transcribe stage demoting `ModelNotInstalled` into a
/// `Skipped` outcome) match against this enum via
/// [`transcribe_window_typed`]. Most callers go through the
/// convenience wrappers ([`transcribe_window`],
/// [`install_speech_model`]) which collapse everything into
/// `ab_core::Error::Stage` — fine for "log and move on" flows.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BridgeError {
    /// `kErrCodeGeneric` (1). Swift caught an error type we
    /// don't classify. Detail in stderr.
    #[error("generic Swift bridge error (see stderr)")]
    Generic,
    /// `kErrCodeFrameworkUnavailable` (2). Apple Intelligence is
    /// disabled / not provisioned on this host.
    #[error("Speech framework unavailable (Apple Intelligence not enabled)")]
    FrameworkUnavailable,
    /// `kErrCodeLocaleUnsupported` (3). `SpeechTranscriber` has
    /// no equivalent for the BCP-47 string we passed.
    #[error("locale not supported by SpeechTranscriber")]
    LocaleUnsupported,
    /// `kErrCodeModelNotInstalled` (4). Status was `.supported`
    /// or `.downloading`, not `.installed`. Daemon should queue
    /// an idle-priority install via [`install_speech_model`].
    #[error("on-device Speech model not installed for this locale")]
    ModelNotInstalled,
    /// `kErrCodeWindowEmpty` (5). `start_secs >= end_secs` or
    /// the requested range fell entirely outside the file.
    #[error("transcribe window is empty / invalid")]
    WindowEmpty,
    /// `kErrCodeNoCompatibleAudioFormat` (6).
    #[error("no audio format compatible with the engine")]
    NoCompatibleAudioFormat,
    /// `kErrCodeReadFailure` (7). `AVAudioFile` open / decode /
    /// `AVAudioConverter` init / conversion failure.
    #[error("audio read or convert failure")]
    ReadFailure,
    /// `kErrCodeEncodeFailure` (8). JSON encode of the segments
    /// failed (extremely rare — would mean a non-UTF8 byte in
    /// the engine's `AttributedString`).
    #[error("payload encode failure")]
    EncodeFailure,
    /// Unknown code. New Swift code without matching Rust
    /// classification. Always log + treat as Generic.
    #[error("unknown bridge error code {0}")]
    UnknownCode(i32),
    /// Bridge accepted the call but the callback was dropped
    /// without firing. Almost certainly a Swift-side panic.
    #[error("Swift dropped the callback without firing it")]
    CallbackDropped,
    /// Buffer pointer was null but the code was `kErrCodeOK`,
    /// or the buffer wasn't valid UTF-8.
    #[error("invalid buffer payload: {0}")]
    InvalidPayload(String),
    /// JSON shape didn't match the expected schema.
    #[error("payload schema mismatch: {0}")]
    PayloadParse(String),
    /// Bridge not linked at build time (non-macOS host or
    /// swiftc missing). Stable error that callers can use to
    /// degrade gracefully.
    #[error("Speech FFI bridge not linked (non-macOS host or swiftc unavailable)")]
    BridgeUnavailable,
    /// `CString` conversion caught a NUL byte in user input
    /// before crossing the FFI boundary.
    #[error("nul byte in FFI input: {0}")]
    NulInInput(String),
}

impl BridgeError {
    /// Map the C ABI `i32` to a typed variant. `0` is the
    /// success code and never reaches this function; callers
    /// should branch on `code == 0` before classifying.
    #[must_use]
    pub const fn from_code(code: i32) -> Self {
        match code {
            1 => Self::Generic,
            2 => Self::FrameworkUnavailable,
            3 => Self::LocaleUnsupported,
            4 => Self::ModelNotInstalled,
            5 => Self::WindowEmpty,
            6 => Self::NoCompatibleAudioFormat,
            7 => Self::ReadFailure,
            8 => Self::EncodeFailure,
            other => Self::UnknownCode(other),
        }
    }
}

impl From<BridgeError> for Error {
    fn from(e: BridgeError) -> Self {
        Self::stage("transcribe", e.to_string())
    }
}

/// Per-locale Speech-model status report returned by
/// [`speech_locale_status`]. The doctor command uses this to
/// surface install / availability state to the user.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct LocaleStatusReport {
    /// `false` when Apple Intelligence is disabled in System
    /// Settings or otherwise unavailable on the host. When
    /// false, the other fields still reflect what the SDK
    /// reports but the doctor presents the framework-
    /// unavailable diagnosis first.
    pub framework_available: bool,
    /// `true` when `SpeechTranscriber.supportedLocale` returns
    /// a non-nil mapping for the input locale. `false` means
    /// the SDK doesn't know this locale at all.
    pub locale_supported: bool,
    /// One of `"installed"` / `"supported"` (=available for
    /// download) / `"downloading"` / `"unsupported"` /
    /// `"unknown"`. Mirrors the `AssetInventory.Status` enum
    /// in the Speech framework.
    pub status: String,
}

/// One transcribed segment.
///
/// Sentence-level when the engine returns sentence boundaries;
/// word-level if a future engine version returns finer
/// granularity (the JSON contract is the same — `text` just
/// gets shorter).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TranscriptSegment {
    /// Start of the segment within the original audio file, in
    /// milliseconds since the file's start (NOT since the
    /// transcribed window's start).
    pub start_ms: u64,
    /// End of the segment in the same coordinate space.
    pub end_ms: u64,
    /// Transcribed text. Already normalised by the engine.
    pub text: String,
    /// Engine-reported confidence in `[0.0, 1.0]`.
    pub confidence: f32,
}

#[cfg(aborg_ai_bridge)]
#[expect(
    unsafe_code,
    reason = "FFI to Swift requires unsafe extern blocks and raw-pointer round-trips through the C callback; safe wrappers exposed by the parent module are the public surface."
)]
mod ffi {
    use std::ffi::{c_char, c_void};
    use std::path::Path;

    use tokio::sync::oneshot;

    use super::{BridgeError, TranscriptSegment};

    /// Internal carrier: the FFI callback delivers a code plus
    /// an optional buffer; we collect them into this struct and
    /// the calling task classifies.
    pub(super) struct FfiResult {
        pub code: i32,
        pub buffer: Option<Vec<u8>>,
    }

    // Symbols exported by `swift/aborg_ai.swift` (see the
    // `@_cdecl(...)` annotations there). Callback signature is
    // `(ctx, data_ptr, len, error_code)` — `error_code == 0`
    // means success and `data_ptr` is the JSON buffer; otherwise
    // `data_ptr` is null and the code identifies the failure
    // (see [`BridgeError::from_code`]).
    unsafe extern "C" {
        fn aborg_transcribe_window(
            input_path: *const c_char,
            start_secs: f64,
            end_secs: f64,
            locale: *const c_char,
            ctx: *mut c_void,
            callback: unsafe extern "C" fn(*mut c_void, *const c_char, usize, i32),
        );
        fn aborg_install_speech_model(
            locale: *const c_char,
            ctx: *mut c_void,
            callback: unsafe extern "C" fn(*mut c_void, *const c_char, usize, i32),
        );
        fn aborg_speech_locale_status(
            locale: *const c_char,
            ctx: *mut c_void,
            callback: unsafe extern "C" fn(*mut c_void, *const c_char, usize, i32),
        );
    }

    /// C ABI callback. Recovers the boxed sender, copies the
    /// payload (if any) into an owned `Vec<u8>`, ships everything
    /// through the oneshot.
    unsafe extern "C" fn on_result(ctx: *mut c_void, ptr: *const c_char, len: usize, code: i32) {
        if ctx.is_null() {
            return;
        }
        // SAFETY: paired with `Box::into_raw` in the caller;
        // Swift returns ctx unchanged exactly once.
        let sender = unsafe { Box::from_raw(ctx.cast::<oneshot::Sender<FfiResult>>()) };
        let buffer = if ptr.is_null() || len == 0 {
            None
        } else {
            // SAFETY: Swift documents `(ptr, len)` as a buffer of
            // exactly `len` bytes valid for the duration of this
            // callback. `to_vec` copies immediately so the buffer
            // lifetime ends with the callback.
            let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
            Some(slice.to_vec())
        };
        let _ = sender.send(FfiResult { code, buffer });
    }

    pub(super) async fn locale_status_impl(
        locale: &str,
    ) -> Result<super::LocaleStatusReport, BridgeError> {
        let locale_c = std::ffi::CString::new(locale)
            .map_err(|e| BridgeError::NulInInput(format!("locale: {e}")))?;
        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();
        // SAFETY: `locale_c` outlives the synchronous Swift call;
        // the Swift Task fires on_result exactly once.
        unsafe {
            aborg_speech_locale_status(locale_c.as_ptr(), ctx, on_result);
        }
        let res = rx.await.map_err(|_| BridgeError::CallbackDropped)?;
        if res.code != 0 {
            return Err(BridgeError::from_code(res.code));
        }
        let bytes = res
            .buffer
            .ok_or_else(|| BridgeError::InvalidPayload("OK code but null buffer".into()))?;
        let s = std::str::from_utf8(&bytes)
            .map_err(|e| BridgeError::InvalidPayload(format!("non-utf8: {e}")))?;
        serde_json::from_str::<super::LocaleStatusReport>(s)
            .map_err(|e| BridgeError::PayloadParse(format!("locale-status: {e}")))
    }

    pub(super) async fn install_speech_model_impl(locale: &str) -> Result<(), BridgeError> {
        let locale_c = std::ffi::CString::new(locale)
            .map_err(|e| BridgeError::NulInInput(format!("locale: {e}")))?;
        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();
        // SAFETY: `locale_c` outlives the synchronous Swift call;
        // the install Task runs detached on the Swift side and
        // only fires `on_result` exactly once.
        unsafe {
            aborg_install_speech_model(locale_c.as_ptr(), ctx, on_result);
        }
        let res = rx.await.map_err(|_| BridgeError::CallbackDropped)?;
        if res.code == 0 {
            Ok(())
        } else {
            Err(BridgeError::from_code(res.code))
        }
    }

    pub(super) async fn transcribe_window_impl(
        input_path: &Path,
        start_secs: f64,
        end_secs: f64,
        locale: &str,
    ) -> Result<Vec<TranscriptSegment>, BridgeError> {
        let path_str = input_path
            .to_str()
            .ok_or_else(|| BridgeError::NulInInput("input_path is not valid UTF-8".into()))?;
        let path_c = std::ffi::CString::new(path_str)
            .map_err(|e| BridgeError::NulInInput(format!("input_path: {e}")))?;
        let locale_c = std::ffi::CString::new(locale)
            .map_err(|e| BridgeError::NulInInput(format!("locale: {e}")))?;

        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();

        // SAFETY: the two `*const c_char` pointers outlive the
        // synchronous portion of the call (their CStrings are
        // not dropped until this fn returns). The `ctx` and
        // callback are paired — Swift fires `on_result` exactly
        // once per call, regardless of success path.
        unsafe {
            aborg_transcribe_window(
                path_c.as_ptr(),
                start_secs,
                end_secs,
                locale_c.as_ptr(),
                ctx,
                on_result,
            );
        }
        let res = rx.await.map_err(|_| BridgeError::CallbackDropped)?;
        if res.code != 0 {
            return Err(BridgeError::from_code(res.code));
        }
        let bytes = res
            .buffer
            .ok_or_else(|| BridgeError::InvalidPayload("OK code but null buffer".into()))?;
        let s = std::str::from_utf8(&bytes)
            .map_err(|e| BridgeError::InvalidPayload(format!("non-utf8: {e}")))?;
        serde_json::from_str::<Vec<TranscriptSegment>>(s)
            .map_err(|e| BridgeError::PayloadParse(format!("segments: {e}")))
    }
}

/// Query the Speech-model install state for a single locale.
///
/// Used by `aborg doctor` to surface per-locale install state
/// without committing to an install (which can take minutes
/// the first time). Returns a [`LocaleStatusReport`] describing
/// the framework availability + locale support + install
/// status.
///
/// # Errors
///
/// See [`BridgeError`] for the variants.
pub async fn speech_locale_status(locale: &str) -> Result<LocaleStatusReport, BridgeError> {
    #[cfg(aborg_ai_bridge)]
    {
        ffi::locale_status_impl(locale).await
    }
    #[cfg(not(aborg_ai_bridge))]
    {
        let _ = locale;
        Err(BridgeError::BridgeUnavailable)
    }
}

/// Typed variant of [`install_speech_model`].
///
/// Returns the raw [`BridgeError`] enum so callers (e.g. the
/// doctor command) can branch on `FrameworkUnavailable` vs.
/// `LocaleUnsupported` without parsing strings.
///
/// # Errors
///
/// See [`BridgeError`].
pub async fn install_speech_model_typed(locale: &str) -> Result<(), BridgeError> {
    #[cfg(aborg_ai_bridge)]
    {
        ffi::install_speech_model_impl(locale).await
    }
    #[cfg(not(aborg_ai_bridge))]
    {
        let _ = locale;
        Err(BridgeError::BridgeUnavailable)
    }
}

/// Convenience wrapper around [`install_speech_model_typed`].
///
/// Collapses every variant into `ab_core::Error::Stage`. Use the
/// typed version when you need to branch on a specific failure
/// (e.g. the idle-install retry path treats
/// `FrameworkUnavailable` as terminal).
///
/// # Errors
///
/// See [`install_speech_model_typed`].
pub async fn install_speech_model(locale: &str) -> Result<()> {
    install_speech_model_typed(locale).await.map_err(Into::into)
}

/// Transcribe `[start_secs, end_secs)` of `input_path` in the
/// given BCP-47 `locale`. Returns segments in the file's original
/// time-base.
///
/// # Errors
///
/// See [`BridgeError`] for the enum of typed failures.
pub async fn transcribe_window_typed(
    input_path: &Path,
    start_secs: f64,
    end_secs: f64,
    locale: &str,
) -> Result<Vec<TranscriptSegment>, BridgeError> {
    #[cfg(aborg_ai_bridge)]
    {
        ffi::transcribe_window_impl(input_path, start_secs, end_secs, locale).await
    }
    #[cfg(not(aborg_ai_bridge))]
    {
        let _ = (input_path, start_secs, end_secs, locale);
        Err(BridgeError::BridgeUnavailable)
    }
}

/// Convenience wrapper around [`transcribe_window_typed`] that
/// collapses everything into `ab_core::Error::Stage`.
///
/// # Errors
///
/// See [`transcribe_window_typed`].
pub async fn transcribe_window(
    input_path: &Path,
    start_secs: f64,
    end_secs: f64,
    locale: &str,
) -> Result<Vec<TranscriptSegment>> {
    transcribe_window_typed(input_path, start_secs, end_secs, locale)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn bogus_path_returns_error_when_bridge_linked() {
        // With the real SpeechAnalyzer body in slice 3A.3, a
        // non-audio path fails in the AVAudioFile open step;
        // the Swift side logs the error to stderr and the
        // callback fires with a null buffer pointer → Rust
        // wrapper translates to `Error::Stage`.
        // On platforms without the bridge, the wrapper short-
        // circuits to Err immediately.
        let result = transcribe_window(&PathBuf::from("/dev/null"), 0.0, 1.0, "en-US").await;
        assert!(
            result.is_err(),
            "bogus path / no bridge must return Err (got {result:?})"
        );
    }

    #[tokio::test]
    async fn input_path_with_nul_byte_is_rejected_cleanly() {
        // The CString-conversion path catches NULs before we
        // ever cross the FFI boundary. Runs on every host.
        let bad = std::ffi::OsString::from("/tmp/a\0b");
        let bad_path = PathBuf::from(bad);
        let r = transcribe_window(&bad_path, 0.0, 1.0, "en-US").await;
        assert!(r.is_err(), "NUL in path must be rejected, got {r:?}");
    }

    #[tokio::test]
    async fn empty_window_returns_error() {
        // start_secs == end_secs is invalid — the engine has
        // no audio to chew on. Verified both bridge-linked and
        // bridge-absent paths.
        let r = transcribe_window(&PathBuf::from("/dev/null"), 1.0, 1.0, "en-US").await;
        assert!(r.is_err(), "zero-length window must Err, got {r:?}");
    }
}
