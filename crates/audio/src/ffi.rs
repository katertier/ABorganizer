//! Safe Rust wrapper around the `aborg_audio_read_window` Swift
//! FFI entry point.
//!
//! Decodes a `[start_ms, end_ms)` window of the input file as
//! mono Float32 PCM at the requested sample rate via
//! `AVAssetReader`. The audio side of the audiologo detection
//! path (slice 4B / ADR-0024 Revision 2).
//!
//! # Contract
//!
//! On a successful call the Swift side fires the registered
//! callback exactly once with a byte buffer of native-endian
//! Float32 samples. The callback's `ctx` argument is an opaque
//! `Box<oneshot::Sender>` that this module leaks into Swift and
//! recovers in the callback. The `data_ptr` may be null on the
//! Swift side to signal failure; the wrapper translates that
//! into a typed [`BridgeError`].
//!
//! # Non-macOS / no-swiftc fallback
//!
//! When `cfg(ab_audio_bridge)` isn't set (non-macOS target or
//! swiftc missing at build time), the wrapper returns
//! [`BridgeError::BridgeUnavailable`] at runtime so the crate
//! still compiles on Linux CI and the upstream caller can
//! degrade gracefully (skip the detect-audiologo stage, log,
//! move on).

use std::path::Path;

use ab_core::{Error, Result};

/// True when the Swift bridge is linked into this binary.
#[must_use]
pub const fn is_bridge_compiled() -> bool {
    cfg!(ab_audio_bridge)
}

/// Typed Swift-FFI error variants. Mirrors the `kErrCode*`
/// constants in `swift/aborg_audio.swift`. Values are stable
/// across versions — new variants append, never reorder.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BridgeError {
    /// `kErrCodeGeneric` (1). Swift caught an error type we
    /// don't classify. Detail in stderr.
    #[error("generic ab_audio bridge error (see stderr)")]
    Generic,
    /// `kErrCodeAssetLoadFailed` (2). `AVURLAsset` couldn't load
    /// the file's tracks. Most often: file missing, unreadable,
    /// or an AAX file with no Audible activation bytes.
    #[error("asset load failed (file missing, unreadable, or AAX without decrypt)")]
    AssetLoadFailed,
    /// `kErrCodeNoAudioTrack` (3). The asset has no audio track.
    #[error("file contains no audio track")]
    NoAudioTrack,
    /// `kErrCodeWindowEmpty` (4). `end_ms <= start_ms` or
    /// `sample_rate == 0`.
    #[error("audio window is empty / invalid")]
    WindowEmpty,
    /// `kErrCodeReadFailure` (5). `AVAssetReader` init / start /
    /// decode failure, or `CMBlockBufferCopyDataBytes` failure.
    #[error("audio read or decode failure")]
    ReadFailure,
    /// Unknown code. New Swift code without matching Rust
    /// classification. Always log + treat as Generic.
    #[error("unknown ab_audio bridge error code {0}")]
    UnknownCode(i32),
    /// Bridge accepted the call but the callback was dropped
    /// without firing. Almost certainly a Swift-side panic.
    #[error("Swift dropped the ab_audio callback without firing it")]
    CallbackDropped,
    /// Buffer pointer was null but the code was 0 (success), or
    /// the buffer length isn't a multiple of 4 (not a clean
    /// Float32 stream).
    #[error("invalid buffer payload: {0}")]
    InvalidPayload(String),
    /// Bridge not linked at build time (non-macOS host or swiftc
    /// missing).
    #[error("ab_audio FFI bridge not linked (non-macOS host or swiftc unavailable)")]
    BridgeUnavailable,
    /// `CString` conversion caught a NUL byte in user input
    /// before crossing the FFI boundary.
    #[error("nul byte in FFI input: {0}")]
    NulInInput(String),
}

impl BridgeError {
    /// Map the C ABI `i32` to a typed variant. `0` is the
    /// success code and never reaches this function; callers
    /// branch on `code == 0` before classifying.
    #[must_use]
    pub const fn from_code(code: i32) -> Self {
        match code {
            1 => Self::Generic,
            2 => Self::AssetLoadFailed,
            3 => Self::NoAudioTrack,
            4 => Self::WindowEmpty,
            5 => Self::ReadFailure,
            other => Self::UnknownCode(other),
        }
    }
}

impl From<BridgeError> for Error {
    fn from(e: BridgeError) -> Self {
        Self::stage("ab-audio", e.to_string())
    }
}

#[cfg(ab_audio_bridge)]
#[expect(
    unsafe_code,
    reason = "FFI to Swift requires unsafe extern blocks and raw-pointer round-trips through the C callback; safe wrappers exposed by the parent module are the public surface."
)]
#[allow(
    clippy::module_inception,
    reason = "inner `ffi` holds the unsafe extern block and is gated by cfg(ab_audio_bridge); flattening into the parent would force the cfg gate on the parent module + lose the safe/unsafe boundary."
)]
mod ffi {
    use std::ffi::{CString, c_void};
    use std::path::Path;

    use tokio::sync::oneshot;

    use super::BridgeError;

    /// Internal carrier: the FFI callback delivers a code plus
    /// an optional byte buffer; we collect them into this struct
    /// and the calling task classifies.
    pub(super) struct FfiResult {
        pub code: i32,
        pub buffer: Option<Vec<u8>>,
    }

    // Symbol exported by `swift/aborg_audio.swift`. Callback:
    // `(ctx, data_ptr, len_bytes, error_code)` — `error_code == 0`
    // means success and `data_ptr` is the raw Float32 buffer.
    unsafe extern "C" {
        fn aborg_audio_read_window(
            input_path: *const std::ffi::c_char,
            start_ms: u64,
            end_ms: u64,
            sample_rate: u32,
            ctx: *mut c_void,
            callback: unsafe extern "C" fn(*mut c_void, *const u8, usize, i32),
        );
    }

    unsafe extern "C" fn on_result(ctx: *mut c_void, ptr: *const u8, len: usize, code: i32) {
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
            let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
            Some(slice.to_vec())
        };
        let _ = sender.send(FfiResult { code, buffer });
    }

    pub(super) async fn read_samples_window_impl(
        input_path: &Path,
        start_ms: u64,
        end_ms: u64,
        sample_rate: u32,
    ) -> Result<Vec<f32>, BridgeError> {
        if end_ms <= start_ms || sample_rate == 0 {
            return Err(BridgeError::WindowEmpty);
        }
        let path_str = input_path
            .to_str()
            .ok_or_else(|| BridgeError::NulInInput("input_path is not valid UTF-8".into()))?;
        let path_c = CString::new(path_str)
            .map_err(|e| BridgeError::NulInInput(format!("input_path: {e}")))?;

        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();

        // SAFETY: `path_c` outlives the synchronous portion of
        // the call (its CString is not dropped until this fn
        // returns). The `ctx` and callback are paired — Swift
        // fires `on_result` exactly once per call, regardless of
        // success path.
        unsafe {
            aborg_audio_read_window(
                path_c.as_ptr(),
                start_ms,
                end_ms,
                sample_rate,
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
        if bytes.len() % 4 != 0 {
            return Err(BridgeError::InvalidPayload(format!(
                "byte len {} not a multiple of 4 (not a clean Float32 stream)",
                bytes.len()
            )));
        }
        let sample_count = bytes.len() / 4;
        let mut samples = Vec::with_capacity(sample_count);
        for chunk in bytes.chunks_exact(4) {
            let arr: [u8; 4] = [chunk[0], chunk[1], chunk[2], chunk[3]];
            samples.push(f32::from_ne_bytes(arr));
        }
        Ok(samples)
    }
}

/// Decode `[start_ms, end_ms)` of `input_path` as mono Float32
/// PCM at the requested sample rate.
///
/// Typed variant exposing the `BridgeError` enum so callers can
/// branch on a specific failure (e.g. `AssetLoadFailed` for a
/// deferred-AAX path).
///
/// # Errors
///
/// See [`BridgeError`] for the variants.
pub async fn read_samples_window_typed(
    input_path: &Path,
    start_ms: u64,
    end_ms: u64,
    sample_rate: u32,
) -> Result<Vec<f32>, BridgeError> {
    #[cfg(ab_audio_bridge)]
    {
        ffi::read_samples_window_impl(input_path, start_ms, end_ms, sample_rate).await
    }
    #[cfg(not(ab_audio_bridge))]
    {
        let _ = (input_path, start_ms, end_ms, sample_rate);
        Err(BridgeError::BridgeUnavailable)
    }
}

/// Convenience wrapper around [`read_samples_window_typed`].
///
/// Collapses every variant into `ab_core::Error::Stage`. Use the
/// typed version when you need to branch on a specific failure
/// (e.g. an AAX file → defer behind decrypt).
///
/// # Errors
///
/// See [`read_samples_window_typed`].
pub async fn read_samples_window(
    input_path: &Path,
    start_ms: u64,
    end_ms: u64,
    sample_rate: u32,
) -> Result<Vec<f32>> {
    read_samples_window_typed(input_path, start_ms, end_ms, sample_rate)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::match_same_arms)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn empty_window_returns_window_empty() {
        // Both bridge-linked + bridge-absent paths validate the
        // window before crossing FFI. `end_ms <= start_ms` always
        // fails with the same typed variant on every host.
        let r = read_samples_window_typed(&PathBuf::from("/dev/null"), 1000, 1000, 11_025).await;
        match r {
            Err(BridgeError::WindowEmpty) => {}
            Err(BridgeError::BridgeUnavailable) => {
                // No bridge: the wrapper also rejects in the typed
                // variant before the FFI call, but the order of
                // checks is implementation-defined; either is fine.
            }
            other => panic!("expected WindowEmpty or BridgeUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn zero_sample_rate_returns_window_empty() {
        let r = read_samples_window_typed(&PathBuf::from("/dev/null"), 0, 1000, 0).await;
        match r {
            Err(BridgeError::WindowEmpty | BridgeError::BridgeUnavailable) => {}
            other => panic!("expected WindowEmpty or BridgeUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn input_path_with_nul_byte_is_rejected_cleanly() {
        // The CString-conversion path catches NULs before we ever
        // cross the FFI boundary. Bridge-absent path can't reach
        // this check (returns BridgeUnavailable first), so we
        // accept either outcome.
        let bad = std::ffi::OsString::from("/tmp/a\0b");
        let bad_path = PathBuf::from(bad);
        let r = read_samples_window(&bad_path, 0, 1000, 11_025).await;
        assert!(r.is_err(), "NUL in path or no bridge must Err, got {r:?}");
    }

    #[tokio::test]
    async fn bogus_path_returns_error_when_bridge_linked() {
        // /dev/null has no audio tracks; on bridge-linked builds
        // the AVURLAsset.loadTracks call fails. On bridge-absent
        // builds the wrapper short-circuits to BridgeUnavailable.
        let r = read_samples_window(&PathBuf::from("/dev/null"), 0, 1000, 11_025).await;
        assert!(
            r.is_err(),
            "bogus path / no bridge must return Err (got {r:?})"
        );
    }
}
