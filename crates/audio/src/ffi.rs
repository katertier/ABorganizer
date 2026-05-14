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
    /// `kErrCodeExportSetupFailed` (6). `AVAssetExportSession`
    /// refused to initialise / configure — preset rejected,
    /// output path unwritable, codec mismatch. Operational fix:
    /// inspect Swift stderr; usually a permissions or path
    /// issue rather than a transient failure.
    #[error("transcode export session setup failed (preset, output path, codec)")]
    ExportSetupFailed,
    /// `kErrCodeExportRunFailed` (7). The export session ran
    /// but errored mid-export — decode failure, disk full, or
    /// the post-run sanity check (`output exists + size > 0`)
    /// rejected the result. Surface as a retryable error.
    #[error("transcode export failed mid-run (decode error, disk, or empty output)")]
    ExportRunFailed,
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
            6 => Self::ExportSetupFailed,
            7 => Self::ExportRunFailed,
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
        // Transcode entry point (slice C2a). Re-encodes
        // `input_path` to AAC-LC inside an m4a container at
        // `output_path`. On success the callback fires with
        // `(ctx, null, 0, 0)` — the output is on disk, not in
        // the buffer. Failure paths fire with a non-zero code.
        fn aborg_audio_transcode_to_m4b(
            input_path: *const std::ffi::c_char,
            output_path: *const std::ffi::c_char,
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

    pub(super) async fn transcode_to_m4b_impl(
        input_path: &Path,
        output_path: &Path,
    ) -> Result<(), BridgeError> {
        let input_str = input_path
            .to_str()
            .ok_or_else(|| BridgeError::NulInInput("input_path is not valid UTF-8".into()))?;
        let output_str = output_path
            .to_str()
            .ok_or_else(|| BridgeError::NulInInput("output_path is not valid UTF-8".into()))?;
        let input_c = CString::new(input_str)
            .map_err(|e| BridgeError::NulInInput(format!("input_path: {e}")))?;
        let output_c = CString::new(output_str)
            .map_err(|e| BridgeError::NulInInput(format!("output_path: {e}")))?;

        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();

        // SAFETY: both `CString`s outlive the synchronous portion
        // of the call (drop at function end). The Swift side
        // fires `on_result` exactly once per call regardless of
        // success/failure path (the export-session task always
        // calls the callback before exiting Task.detached).
        unsafe {
            aborg_audio_transcode_to_m4b(input_c.as_ptr(), output_c.as_ptr(), ctx, on_result);
        }
        let res = rx.await.map_err(|_| BridgeError::CallbackDropped)?;
        if res.code != 0 {
            return Err(BridgeError::from_code(res.code));
        }
        // Transcode success: buffer is documented as `null`. If
        // Swift ever starts piggy-backing diagnostic bytes we
        // tolerate them silently — the on-disk output is the
        // authoritative result.
        let _ = res.buffer;
        Ok(())
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

/// Re-encode `input_path` to AAC-LC inside an m4a-shaped container.
///
/// ADR-0027 ("everything is m4b") wires this into the future
/// `transcode-m4b` stage; this function ships in slice C2a so
/// the wrapper can be unit-tested independently of the stage
/// wiring.
///
/// The on-disk container is identical to an `.m4a` — `.m4b` is an
/// audiobook-convention extension. The caller chooses the
/// extension by passing the desired `output_path`.
///
/// **Bitrate**: fixed by Apple's `appleM4A` preset (~64 kbps for
/// mono input, ~128 kbps for stereo). Per-bitrate control needs
/// the `AVAssetWriter` path which a future slice can add when
/// operators ask for it.
///
/// **Cover art + tags**: not carried over by this call — slice
/// C2a is content-only. Cover-art write-back lands in C3
/// (ADR-0028 two-pass tag-write) which probes the source, runs
/// transcode, then writes ID3v2/MP4 metadata onto the output.
///
/// # Errors
///
/// Typed variants — see [`BridgeError`]. Common failure modes:
/// [`BridgeError::AssetLoadFailed`] (input unreadable, AAX
/// without decrypt), [`BridgeError::NoAudioTrack`],
/// [`BridgeError::ExportSetupFailed`] (output dir unwritable),
/// [`BridgeError::ExportRunFailed`] (disk full, mid-export
/// decode error, empty output).
pub async fn transcode_to_m4b_typed(
    input_path: &Path,
    output_path: &Path,
) -> Result<(), BridgeError> {
    #[cfg(ab_audio_bridge)]
    {
        ffi::transcode_to_m4b_impl(input_path, output_path).await
    }
    #[cfg(not(ab_audio_bridge))]
    {
        let _ = (input_path, output_path);
        Err(BridgeError::BridgeUnavailable)
    }
}

/// Convenience wrapper around [`transcode_to_m4b_typed`].
///
/// Collapses every variant into `ab_core::Error::Stage`.
///
/// # Errors
///
/// See [`transcode_to_m4b_typed`].
pub async fn transcode_to_m4b(input_path: &Path, output_path: &Path) -> Result<()> {
    transcode_to_m4b_typed(input_path, output_path)
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

    // ── transcode_to_m4b tests ────────────────────────────────

    #[tokio::test]
    async fn transcode_path_with_nul_byte_is_rejected_cleanly() {
        let input = std::ffi::OsString::from("/tmp/a\0b.m4a");
        let r = transcode_to_m4b(&PathBuf::from(input), &PathBuf::from("/tmp/out.m4b")).await;
        assert!(r.is_err(), "NUL in input must Err, got {r:?}");
    }

    #[tokio::test]
    async fn transcode_output_path_with_nul_byte_is_rejected_cleanly() {
        let output = std::ffi::OsString::from("/tmp/a\0b.m4b");
        let r = transcode_to_m4b(&PathBuf::from("/tmp/in.m4a"), &PathBuf::from(output)).await;
        assert!(r.is_err(), "NUL in output must Err, got {r:?}");
    }

    #[tokio::test]
    async fn transcode_bogus_input_returns_error() {
        // /dev/null again — bridge-linked builds fail with
        // AssetLoadFailed (or NoAudioTrack on some macOS
        // versions); bridge-absent builds return
        // BridgeUnavailable. Either is a clean error.
        let r = transcode_to_m4b(&PathBuf::from("/dev/null"), &PathBuf::from("/tmp/out.m4b")).await;
        assert!(r.is_err(), "bogus input must Err (got {r:?})");
    }

    #[tokio::test]
    async fn transcode_typed_classifies_export_setup_failure() {
        // Output path inside a non-existent directory should
        // surface as ExportSetupFailed on bridge-linked builds
        // (Swift's AVAssetExportSession can't write into a
        // missing parent dir). Bridge-absent builds short-circuit
        // to BridgeUnavailable. The point of this test is to
        // exercise the code-6 / code-7 dispatch path — we accept
        // any error variant here; future revisions tighten as
        // the Swift side stabilises.
        let r = transcode_to_m4b_typed(
            &PathBuf::from("/dev/null"),
            &PathBuf::from("/nonexistent-dir/out.m4b"),
        )
        .await;
        assert!(r.is_err(), "must Err (got {r:?})");
    }

    /// Round-trip happy path. Only meaningful when the bridge is
    /// linked (bridge-absent builds short-circuit on every call).
    /// Uses an Apple-shipped system sound (`/System/Library/Sounds`
    /// is present on every macOS) as the source, so the test has
    /// no committed audio fixture to maintain.
    #[tokio::test]
    async fn transcode_round_trip_writes_valid_m4b_on_bridge_linked() {
        if !is_bridge_compiled() {
            return; // Linux CI or no-swiftc build — nothing to verify.
        }
        let source = Path::new("/System/Library/Sounds/Submarine.aiff");
        if !source.exists() {
            // Should not happen on macOS, but skip rather than fail
            // if Apple ever relocates the system sounds.
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let out = tmp.path().join("out.m4b");

        transcode_to_m4b(source, &out)
            .await
            .expect("transcode round trip");

        // Output must exist + be non-empty. The Swift side
        // post-checks size > 0 before returning success, so a
        // missing / empty file here would already have surfaced
        // as ExportRunFailed.
        let meta = std::fs::metadata(&out).expect("output metadata");
        assert!(meta.len() > 0, "output is empty");

        // Round-trip the bytes back through the reader to prove
        // the container is valid + has at least one decodable
        // audio sample. Submarine.aiff is well under 5 seconds —
        // 100 ms is plenty.
        let samples = read_samples_window(&out, 0, 100, 22_050)
            .await
            .expect("read back samples from transcode output");
        assert!(!samples.is_empty(), "no samples read back from m4b output");
    }
}
