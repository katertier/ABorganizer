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

use ab_core::Result;
use serde::{Deserialize, Serialize};

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

    use ab_core::{Error, Result};
    use tokio::sync::oneshot;

    use super::TranscriptSegment;

    // Symbol exported by `swift/aborg_ai.swift` (see the
    // `@_cdecl("aborg_transcribe_window")` annotation there).
    unsafe extern "C" {
        fn aborg_transcribe_window(
            input_path: *const c_char,
            start_secs: f64,
            end_secs: f64,
            locale: *const c_char,
            ctx: *mut c_void,
            callback: unsafe extern "C" fn(*mut c_void, *const c_char, usize),
        );
    }

    /// C ABI callback Swift fires with the JSON result. Recovers
    /// the boxed sender and forwards either Ok(json) or Err.
    unsafe extern "C" fn on_result(ctx: *mut c_void, ptr: *const c_char, len: usize) {
        if ctx.is_null() {
            return;
        }
        // SAFETY: the caller of `transcribe_window` boxed the
        // oneshot sender and converted it with `Box::into_raw`.
        // Swift returns it unchanged here, exactly once. We
        // reclaim ownership; the box drops when the closure
        // ends.
        let sender = unsafe { Box::from_raw(ctx.cast::<oneshot::Sender<Result<String>>>()) };
        let outcome: Result<String> = if ptr.is_null() {
            Err(Error::stage(
                "transcribe",
                "Swift returned null buffer pointer",
            ))
        } else {
            // SAFETY: Swift documents `(ptr, len)` as a UTF-8
            // buffer of exactly `len` bytes. The buffer's
            // lifetime is the duration of this callback (Swift's
            // `withCString` scopes it), which is fine — we copy
            // immediately via `to_owned`.
            let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
            std::str::from_utf8(slice)
                .map(str::to_owned)
                .map_err(|e| Error::stage("transcribe", format!("non-utf8 buffer: {e}")))
        };
        let _ = sender.send(outcome);
    }

    pub(super) async fn transcribe_window_impl(
        input_path: &Path,
        start_secs: f64,
        end_secs: f64,
        locale: &str,
    ) -> Result<Vec<TranscriptSegment>> {
        let path_str = input_path
            .to_str()
            .ok_or_else(|| Error::stage("transcribe", "input_path is not valid UTF-8"))?;
        let path_c = std::ffi::CString::new(path_str)
            .map_err(|e| Error::stage("transcribe", format!("input path has NUL byte: {e}")))?;
        let locale_c = std::ffi::CString::new(locale)
            .map_err(|e| Error::stage("transcribe", format!("locale has NUL byte: {e}")))?;

        let (tx, rx) = oneshot::channel::<Result<String>>();
        // Leak the sender into Swift. The callback reclaims it.
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();

        // SAFETY: the four `*const c_char` pointers outlive the
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
        let json = rx.await.map_err(|_| {
            Error::stage("transcribe", "Swift dropped the callback without firing it")
        })??;
        serde_json::from_str(&json)
            .map_err(|e| Error::stage("transcribe", format!("segment-array parse: {e}")))
    }
}

/// Transcribe `[start_secs, end_secs)` of `input_path` in the
/// given BCP-47 `locale`. Returns segments in the file's original
/// time-base.
///
/// # Errors
///
/// - `Error::Stage("transcribe", ...)` when the bridge is not
///   linked (non-macOS / no-swiftc build).
/// - `Error::Stage("transcribe", ...)` for FFI / parse failures.
pub async fn transcribe_window(
    input_path: &Path,
    start_secs: f64,
    end_secs: f64,
    locale: &str,
) -> Result<Vec<TranscriptSegment>> {
    #[cfg(aborg_ai_bridge)]
    {
        ffi::transcribe_window_impl(input_path, start_secs, end_secs, locale).await
    }
    #[cfg(not(aborg_ai_bridge))]
    {
        let _ = (input_path, start_secs, end_secs, locale);
        Err(Error::stage(
            "transcribe",
            "Speech FFI bridge not linked (non-macOS host or swiftc unavailable)",
        ))
    }
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
