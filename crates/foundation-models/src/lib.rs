//! Apple Intelligence Foundation Models bridge.
//!
//! Thin Rust wrapper over the Swift FFI in
//! `swift/aborg_fm.swift`, compiled to a static lib by
//! `build.rs` and linked in at build time.
//!
//! Two public surfaces:
//!
//! 1. [`status`] — checks whether the on-device model is usable
//!    on this host (Apple Intelligence enabled, device
//!    eligible, model ready). Used by `aborg doctor llm` and by
//!    extractor stages that fail fast when the model can't run.
//! 2. [`complete`] — single-shot prompt → text completion. The
//!    extractor side is responsible for prompt shape (system /
//!    few-shot / user) and for parsing the response (typically
//!    JSON we ask the model to emit).
//!
//! Both calls return typed [`BridgeError`] variants — no string-
//! matching at call sites. The bridge degrades to
//! `BridgeUnavailable` when:
//!
//! * compiled on a non-macOS target,
//! * built on macOS with no `swiftc` on PATH,
//! * built on a macOS SDK without `FoundationModels.framework`,
//! * run on a macOS host below 26.0.
//!
//! The build script (`build.rs`) emits `cfg(aborg_fm_bridge)`
//! when the static lib is produced; otherwise the Rust impls
//! degrade to `Err(BridgeUnavailable)` at runtime.
//!
//! No `ab-db` / `ab-pipeline` deps — pipeline stages that
//! consume this surface live in [`ab_llm_extractors`]. The
//! separation is the point of this crate: a CLI tool or test
//! harness can call `complete()` without dragging the SQL
//! machinery in.

#![cfg_attr(docsrs, feature(doc_cfg))]

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Knobs passed to a single generation call.
///
/// Replaces the bare `max_tokens: usize` arg the surface used
/// previously. Added in the AI-improvements cross-reference with
/// `bbrangeo/apple_ai` so the DNA-tag extractor can request
/// deterministic output (`temperature = Some(0.0)`) while the
/// description / story-arc / characters extractors stay at the
/// framework default (`temperature = None`) for creative variety.
///
/// `temperature = None` → the Swift bridge constructs
/// `GenerationOptions(maximumResponseTokens:)` only, leaving
/// Apple's default sampling alone. `temperature = Some(t)` →
/// the bridge constructs
/// `GenerationOptions(temperature:, maximumResponseTokens:)`.
#[derive(Debug, Clone, Copy)]
pub struct GenerationOptions {
    /// Soft cap on response tokens — passed to
    /// `GenerationOptions.maximumResponseTokens`. The framework
    /// may stop earlier on EOS.
    pub max_tokens: usize,
    /// Sampling temperature in the framework's range (Apple
    /// documents 0.0 ≤ t ≤ 2.0). `None` keeps the framework
    /// default; `Some(0.0)` is deterministic-greedy.
    pub temperature: Option<f64>,
}

impl GenerationOptions {
    /// Build the no-temperature baseline (framework default).
    #[must_use]
    pub const fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            temperature: None,
        }
    }

    /// Build with an explicit temperature.
    #[must_use]
    pub const fn with_temperature(max_tokens: usize, temperature: f64) -> Self {
        Self {
            max_tokens,
            temperature: Some(temperature),
        }
    }
}

impl Default for GenerationOptions {
    /// 512-token budget, framework-default sampling. Reasonable
    /// for a one-shot ask-the-model-a-yes-or-no probe; concrete
    /// extractors tend to override `max_tokens` to their own
    /// tunable.
    fn default() -> Self {
        Self::new(512)
    }
}

/// Reason the on-device LLM isn't usable.
///
/// Mirrors `SystemLanguageModel.Availability.UnavailabilityReason`
/// from `FoundationModels.framework` plus a small set of
/// bridge-level reasons (framework missing, OS below 26, etc.)
/// that don't have a one-to-one Apple equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// User has Apple Intelligence disabled in System Settings.
    AppleIntelligenceNotEnabled,
    /// Mac model / chip can't run the on-device model at all.
    DeviceNotEligible,
    /// SDK present, hardware eligible, but model assets not
    /// downloaded yet (the system downloads them on first
    /// enable; can take ten or twenty minutes on a fresh
    /// install).
    ModelNotReady,
    /// `FoundationModels.framework` was missing at build time
    /// or `swiftc` couldn't compile the bridge.
    FrameworkNotBuilt,
    /// macOS version is below 26.0 (Tahoe). Foundation Models
    /// is macOS 26+ only.
    MacosBelow26,
    /// SDK returned an availability case we don't yet handle.
    /// Treated as unavailable; the bridge logs the raw token
    /// to stderr.
    UnknownAvailability,
}

/// Result of [`status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// `true` only when `SystemLanguageModel.default.availability`
    /// is `.available`.
    pub available: bool,
    /// When `available == false`, this carries the typed reason.
    /// `None` when `available == true`.
    pub reason: Option<UnavailableReason>,
}

/// Typed errors from the Foundation Models bridge.
///
/// The numeric codes are kept in sync with `swift/aborg_fm.swift`
/// — change them there too if you renumber.
#[derive(Debug, Error)]
pub enum BridgeError {
    /// The bridge isn't compiled in (non-macOS, no swiftc, or
    /// `FoundationModels.framework` missing from the SDK).
    #[error("Foundation Models bridge unavailable")]
    BridgeUnavailable,
    /// The host doesn't have an Apple-Intelligence-capable
    /// model state. Use [`status`] for the typed reason.
    #[error("model unavailable: {0}")]
    ModelUnavailable(&'static str),
    /// User disabled Apple Intelligence.
    #[error("Apple Intelligence is not enabled in System Settings")]
    AppleIntelligenceDisabled,
    /// Hardware can't run the model.
    #[error("device not eligible for Apple Intelligence")]
    DeviceNotEligible,
    /// Caller passed an empty prompt.
    #[error("empty prompt")]
    PromptEmpty,
    /// The model returned an error during generation.
    #[error("generation failed: {0}")]
    GenerationFailed(String),
    /// The bridge couldn't parse / encode the FFI payload.
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    /// Generic Swift-side error we don't have a typed variant for.
    /// Detail goes to the Swift stderr log; this end gets only
    /// the code.
    #[error("Foundation Models bridge: generic error")]
    Generic,
    /// Input held a NUL byte (`CString` conversion rejected it).
    #[error("NUL byte in input: {0}")]
    NulInInput(String),
    /// FFI callback was dropped without firing — should be
    /// impossible per contract; if it happens, it's a Swift bug.
    #[error("callback dropped without firing")]
    CallbackDropped,
    /// `complete_structured` was called with a `schema_json` that
    /// is not parseable JSON or isn't a JSON object at the top
    /// level. Caller bug — fix the schema text.
    #[error("schema JSON failed to parse")]
    SchemaParseFailure,
    /// `complete_structured`'s schema parsed, but it used a JSON
    /// Schema feature the bridge doesn't yet support
    /// (`oneOf`, `$ref`, `enum`, etc.). See
    /// `swift/aborg_fm.swift::buildDynamicSchema` for the
    /// supported subset.
    #[error("schema uses a shape the bridge doesn't yet support")]
    SchemaUnsupportedShape,
}

impl BridgeError {
    /// Map a Swift-side error code to a `BridgeError`. The
    /// numeric values are defined in `swift/aborg_fm.swift`.
    ///
    /// Only called from inside the `cfg(aborg_fm_bridge)`-gated
    /// `ffi` module. On hosts where the bridge isn't built
    /// (non-macOS, no swiftc, macOS without `FoundationModels` —
    /// e.g. macos-14 CI), this function has no callers and
    /// clippy `dead_code` would fire; the `allow` is the
    /// surgical fix.
    #[allow(
        dead_code,
        reason = "Used only from the cfg(aborg_fm_bridge)-gated ffi module."
    )]
    fn from_code(code: i32) -> Self {
        match code {
            2 => Self::BridgeUnavailable,
            20 => Self::ModelUnavailable("modelNotReady"),
            21 => Self::AppleIntelligenceDisabled,
            22 => Self::DeviceNotEligible,
            23 => Self::PromptEmpty,
            24 => Self::GenerationFailed("(see Swift stderr)".into()),
            25 => Self::SchemaParseFailure,
            26 => Self::SchemaUnsupportedShape,
            _ => Self::Generic,
        }
    }
}

/// Probe the on-device LLM's availability. Returns a typed
/// [`StatusReport`] without committing to a generation round-trip.
///
/// # Errors
///
/// Returns [`BridgeError::BridgeUnavailable`] when the Swift
/// bridge wasn't compiled in (non-macOS, no swiftc, or framework
/// missing). All "user-fixable" reasons (Apple Intelligence
/// disabled, device not eligible, model still downloading)
/// arrive in `Ok(StatusReport { available: false, reason: Some(..) })`,
/// not as `Err` — the doctor wants those split.
#[allow(
    clippy::unused_async,
    reason = "The async signature stays uniform across hosts. On macOS 26 (cfg(aborg_fm_bridge)) the body awaits the FFI impl; on hosts where swiftc / FoundationModels.framework isn't available the body returns synchronously. Clippy only sees the no-bridge branch on the latter (e.g. macos-14 CI) and lints the unused async — but callers depend on the .await for the macOS 26 path."
)]
pub async fn status() -> Result<StatusReport, BridgeError> {
    #[cfg(aborg_fm_bridge)]
    {
        ffi::status_impl().await
    }
    #[cfg(not(aborg_fm_bridge))]
    {
        Ok(StatusReport {
            available: false,
            reason: Some(UnavailableReason::FrameworkNotBuilt),
        })
    }
}

/// List the BCP-47 locales the on-device Foundation Models accepts.
///
/// Returned vector is sorted (alphabetic, lower-cased). When the
/// bridge isn't compiled in (non-macOS, no swiftc, framework
/// missing) this returns an empty vector — callers that need
/// "supported on this host" should check that vector + the
/// [`status`] result together.
///
/// Used by `aborg doctor llm` to surface "your `library_locale`
/// isn't supported by Apple Intelligence yet" diagnostics before
/// the user hits a runtime generation failure.
///
/// # Errors
///
/// Variants of [`BridgeError`]: [`BridgeError::BridgeUnavailable`]
/// when the bridge isn't compiled in — other failure modes
/// (parse / FFI) come back as [`BridgeError::InvalidPayload`].
#[allow(
    clippy::unused_async,
    reason = "Uniform async surface — see status() for the rationale."
)]
pub async fn supported_locales() -> Result<Vec<String>, BridgeError> {
    #[cfg(aborg_fm_bridge)]
    {
        ffi::supported_locales_impl().await
    }
    #[cfg(not(aborg_fm_bridge))]
    {
        Err(BridgeError::BridgeUnavailable)
    }
}

/// Run a one-shot text completion against the on-device LLM.
///
/// [`GenerationOptions::max_tokens`] is a soft budget passed
/// straight to `GenerationOptions.maximumResponseTokens` — the
/// framework may stop earlier on its own EOS signal.
/// [`GenerationOptions::temperature`] (when `Some`) is passed to
/// `GenerationOptions.temperature`; `None` keeps the framework
/// default.
///
/// # Errors
///
/// Variants of [`BridgeError`]: [`BridgeError::BridgeUnavailable`]
/// when the bridge isn't compiled in;
/// [`BridgeError::AppleIntelligenceDisabled`] /
/// [`BridgeError::DeviceNotEligible`] /
/// [`BridgeError::ModelUnavailable`] when the host can't run
/// the model; [`BridgeError::GenerationFailed`] when the model
/// raises an error mid-generation.
#[allow(
    clippy::unused_async,
    reason = "Uniform async surface — see status() for the rationale."
)]
pub async fn complete(prompt: &str, options: &GenerationOptions) -> Result<String, BridgeError> {
    #[cfg(aborg_fm_bridge)]
    {
        ffi::complete_impl(prompt, options).await
    }
    #[cfg(not(aborg_fm_bridge))]
    {
        let _ = (prompt, options);
        Err(BridgeError::BridgeUnavailable)
    }
}

/// Run a one-shot completion against the on-device LLM that is
/// constrained to produce JSON matching a caller-supplied schema.
///
/// `schema_json` is a JSON-Schema-like document; the bridge maps
/// it to a `DynamicGenerationSchema` and passes it to
/// `session.respond(to:, schema:, includeSchemaInPrompt: true,
/// options:)`. The framework converts the schema into a logits
/// constraint at generation time, so the model can't emit
/// off-schema tokens.
///
/// Supported JSON-Schema shapes (see
/// `swift/aborg_fm.swift::buildDynamicSchema`):
///
/// * `"type": "object"` with a `properties` map and optional
///   `required` array — children are recursed into.
/// * Primitive `"type": "string" | "integer" | "number" | "boolean"`.
/// * `"type": "array"` with an `items` sub-schema.
///
/// Unsupported (rejected with [`BridgeError::SchemaUnsupportedShape`]):
/// `oneOf`, `anyOf`, `allOf`, `enum`, `$ref`, tuple-style array
/// `items`. Add these in a follow-up slice when an extractor needs
/// them.
///
/// Returns the raw JSON string the model produced — caller is
/// responsible for `serde_json::from_str::<MySchema>(&s)`. We
/// don't typed-decode here so this crate stays free of the
/// extractor's domain types.
///
/// # Errors
///
/// In addition to all variants documented on [`complete`]:
/// [`BridgeError::SchemaParseFailure`] when `schema_json` isn't
/// parseable JSON / isn't a top-level object;
/// [`BridgeError::SchemaUnsupportedShape`] when it uses a JSON
/// Schema feature the bridge doesn't yet handle.
#[allow(
    clippy::unused_async,
    reason = "Uniform async surface — see status() for the rationale."
)]
pub async fn complete_structured(
    prompt: &str,
    schema_json: &str,
    options: &GenerationOptions,
) -> Result<String, BridgeError> {
    #[cfg(aborg_fm_bridge)]
    {
        ffi::complete_structured_impl(prompt, schema_json, options).await
    }
    #[cfg(not(aborg_fm_bridge))]
    {
        let _ = (prompt, schema_json, options);
        Err(BridgeError::BridgeUnavailable)
    }
}

// ── FFI ─────────────────────────────────────────────────────────────
//
// The whole `ffi` module is gated on `cfg(aborg_fm_bridge)`. When
// the bridge isn't compiled in, the `extern "C"` block + the
// `_impl` fns aren't defined and the wrappers above return
// `BridgeUnavailable` synchronously.

#[cfg(aborg_fm_bridge)]
#[expect(
    unsafe_code,
    reason = "FFI to Swift requires unsafe extern blocks and raw-pointer round-trips through the C callback; safe wrappers exposed by the parent module are the public surface."
)]
#[expect(
    clippy::redundant_pub_crate,
    reason = "pub(super) is the correct visibility here: items are reached via the safe-wrapper fns in the parent (lib.rs). At one level of nesting clippy thinks this is equivalent to pub(crate), but pub on a private module trips unreachable_pub. pub(super) keeps the intent explicit."
)]
mod ffi {
    //! Raw FFI surface. See `swift/aborg_fm.swift` for the
    //! callback contract. Each Rust wrapper:
    //!
    //!   1. Allocates a oneshot channel,
    //!   2. Boxes the Sender into a raw pointer (`ctx`),
    //!   3. Hands the C callback to the Swift entry,
    //!   4. Awaits the result on the Receiver,
    //!   5. Decodes the (code, payload) pair into a typed result.

    use std::ffi::{CStr, CString, c_char, c_void};

    use tokio::sync::oneshot;

    use super::{BridgeError, StatusReport};

    /// One-shot result shipped through the boxed sender.
    /// `buffer` is `None` for error or success-with-no-payload.
    struct FfiResult {
        code: i32,
        buffer: Option<Vec<u8>>,
    }

    /// Common C callback: deserialise (ptr, len, code) → `FfiResult`,
    /// then send through the boxed oneshot sender (which we
    /// retake ownership of from the ctx pointer).
    ///
    /// # Safety
    ///
    /// `ctx` must be a pointer produced by
    /// `Box::into_raw(Box::new(tx))` where `tx` is a
    /// `oneshot::Sender<FfiResult>`. Swift calls this exactly
    /// once per FFI entry per contract.
    extern "C" fn on_result(ctx: *mut c_void, ptr: *const c_char, len: usize, code: i32) {
        // SAFETY: ctx is a raw pointer to a Box<Sender>. We
        // take ownership back so the Sender gets dropped after
        // .send() — the Swift side is contractually one-shot.
        let tx = unsafe { Box::from_raw(ctx.cast::<oneshot::Sender<FfiResult>>()) };
        let buffer = if ptr.is_null() || len == 0 {
            None
        } else {
            // SAFETY: Swift hands us a UTF-8-encoded buffer that
            // lives until the callback returns. Copy out before
            // the call site frees its Data backing.
            let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
            Some(slice.to_vec())
        };
        // If the receiver was dropped (caller cancelled), .send
        // returns Err; nothing to do here.
        let _ = tx.send(FfiResult { code, buffer });
    }

    unsafe extern "C" {
        /// Probe Foundation Models availability. Emits a JSON
        /// `{available, reason}` blob via the callback.
        fn aborg_fm_status(
            ctx: *mut c_void,
            callback: extern "C" fn(*mut c_void, *const c_char, usize, i32),
        );

        /// One-shot completion. Callback receives the response
        /// text as UTF-8.
        ///
        /// `temperature` is the sampling temperature passed to
        /// `GenerationOptions.temperature` when `use_temperature
        /// != 0`. When `use_temperature == 0`, the bridge omits
        /// the field so Apple's default sampling stays in
        /// effect. Two-field encoding instead of a `NaN` sentinel
        /// keeps the contract typed (clippy/rust hates `NaN`
        /// equality and the Swift side parses booleans more
        /// naturally than `NaN` bit-patterns).
        fn aborg_fm_complete(
            prompt: *const c_char,
            max_tokens: usize,
            temperature: f64,
            use_temperature: i32,
            ctx: *mut c_void,
            callback: extern "C" fn(*mut c_void, *const c_char, usize, i32),
        );

        /// Enumerate the BCP-47 locales the on-device model
        /// accepts. Emits a JSON `{locales: [...]}` blob via the
        /// callback.
        fn aborg_fm_supported_languages(
            ctx: *mut c_void,
            callback: extern "C" fn(*mut c_void, *const c_char, usize, i32),
        );

        /// Schema-constrained completion. The Swift side maps
        /// `schema_json` to a `DynamicGenerationSchema`, passes
        /// it to `session.respond(to:, schema:, ...)`, and emits
        /// `response.content.jsonString` via the callback.
        fn aborg_fm_complete_structured(
            prompt: *const c_char,
            schema_json: *const c_char,
            max_tokens: usize,
            temperature: f64,
            use_temperature: i32,
            ctx: *mut c_void,
            callback: extern "C" fn(*mut c_void, *const c_char, usize, i32),
        );
    }

    /// Raw JSON shape emitted by `aborg_fm_status`. The string
    /// `reason` token vocabulary is defined in
    /// `swift/aborg_fm.swift`.
    #[derive(serde::Deserialize)]
    struct StatusJson {
        available: bool,
        reason: String,
    }

    pub(super) async fn status_impl() -> Result<StatusReport, BridgeError> {
        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();
        // SAFETY: ctx pairs with the on_result callback. Swift
        // fires the callback exactly once.
        unsafe { aborg_fm_status(ctx, on_result) };
        let res = rx.await.map_err(|_| BridgeError::CallbackDropped)?;
        if res.code != 0 {
            return Err(BridgeError::from_code(res.code));
        }
        let bytes = res
            .buffer
            .ok_or_else(|| BridgeError::InvalidPayload("OK code but null buffer".into()))?;
        let s = std::str::from_utf8(&bytes)
            .map_err(|e| BridgeError::InvalidPayload(format!("non-utf8: {e}")))?;
        let json: StatusJson = serde_json::from_str(s)
            .map_err(|e| BridgeError::InvalidPayload(format!("status json: {e}")))?;
        let reason = match json.reason.as_str() {
            "available" => None,
            "apple_intelligence_not_enabled" => {
                Some(super::UnavailableReason::AppleIntelligenceNotEnabled)
            }
            "device_not_eligible" => Some(super::UnavailableReason::DeviceNotEligible),
            "model_not_ready" => Some(super::UnavailableReason::ModelNotReady),
            "framework_not_built" => Some(super::UnavailableReason::FrameworkNotBuilt),
            "macos_below_26" => Some(super::UnavailableReason::MacosBelow26),
            _ => Some(super::UnavailableReason::UnknownAvailability),
        };
        Ok(StatusReport {
            available: json.available,
            reason,
        })
    }

    /// Raw JSON shape emitted by `aborg_fm_supported_languages`.
    #[derive(serde::Deserialize)]
    struct SupportedLanguagesJson {
        locales: Vec<String>,
    }

    pub(super) async fn supported_locales_impl() -> Result<Vec<String>, BridgeError> {
        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();
        // SAFETY: ctx pairs with on_result; Swift fires exactly once.
        unsafe { aborg_fm_supported_languages(ctx, on_result) };
        let res = rx.await.map_err(|_| BridgeError::CallbackDropped)?;
        if res.code != 0 {
            return Err(BridgeError::from_code(res.code));
        }
        let bytes = res
            .buffer
            .ok_or_else(|| BridgeError::InvalidPayload("OK code but null buffer".into()))?;
        let s = std::str::from_utf8(&bytes)
            .map_err(|e| BridgeError::InvalidPayload(format!("non-utf8: {e}")))?;
        let json: SupportedLanguagesJson = serde_json::from_str(s)
            .map_err(|e| BridgeError::InvalidPayload(format!("supported_languages json: {e}")))?;
        Ok(json.locales)
    }

    /// Split a [`super::GenerationOptions`] into the
    /// `(temperature, use_temperature)` FFI pair. `None` → `(0.0, 0)`;
    /// `Some(t)` → `(t, 1)`. The Swift side reads `use_temperature`
    /// first and ignores the `temperature` payload when it's 0.
    const fn split_temperature(opts: &super::GenerationOptions) -> (f64, i32) {
        match opts.temperature {
            Some(t) => (t, 1),
            None => (0.0, 0),
        }
    }

    pub(super) async fn complete_impl(
        prompt: &str,
        options: &super::GenerationOptions,
    ) -> Result<String, BridgeError> {
        let c_prompt =
            CString::new(prompt).map_err(|e| BridgeError::NulInInput(format!("prompt: {e}")))?;
        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();
        let (temperature, use_temperature) = split_temperature(options);
        // SAFETY: ctx pairs with on_result; CString outlives
        // the call into Swift (we hold it until the await
        // returns). Swift fires the callback exactly once.
        unsafe {
            aborg_fm_complete(
                c_prompt.as_ptr(),
                options.max_tokens,
                temperature,
                use_temperature,
                ctx,
                on_result,
            );
        }
        // CStr is preserved by c_prompt living until rx.await
        // resolves; Swift copies the bytes synchronously
        // before kicking off its Task.
        let _ = CStr::from_bytes_with_nul(c_prompt.as_bytes_with_nul());
        let res = rx.await.map_err(|_| BridgeError::CallbackDropped)?;
        if res.code != 0 {
            return Err(BridgeError::from_code(res.code));
        }
        let bytes = res
            .buffer
            .ok_or_else(|| BridgeError::InvalidPayload("OK code but null buffer".into()))?;
        String::from_utf8(bytes).map_err(|e| BridgeError::InvalidPayload(format!("utf8: {e}")))
    }

    pub(super) async fn complete_structured_impl(
        prompt: &str,
        schema_json: &str,
        options: &super::GenerationOptions,
    ) -> Result<String, BridgeError> {
        let c_prompt =
            CString::new(prompt).map_err(|e| BridgeError::NulInInput(format!("prompt: {e}")))?;
        let c_schema = CString::new(schema_json)
            .map_err(|e| BridgeError::NulInInput(format!("schema_json: {e}")))?;
        let (tx, rx) = oneshot::channel::<FfiResult>();
        let ctx = Box::into_raw(Box::new(tx)).cast::<c_void>();
        let (temperature, use_temperature) = split_temperature(options);
        // SAFETY: ctx pairs with on_result; both CStrings outlive
        // the call into Swift (held until the await returns) and
        // Swift copies them synchronously before kicking off its
        // Task. Swift fires the callback exactly once.
        unsafe {
            aborg_fm_complete_structured(
                c_prompt.as_ptr(),
                c_schema.as_ptr(),
                options.max_tokens,
                temperature,
                use_temperature,
                ctx,
                on_result,
            );
        }
        // Keep the CStrings live across the await — defeats any
        // lifetime-inference shortcut that would let LLVM elide
        // them after the unsafe block.
        let _ = CStr::from_bytes_with_nul(c_prompt.as_bytes_with_nul());
        let _ = CStr::from_bytes_with_nul(c_schema.as_bytes_with_nul());
        let res = rx.await.map_err(|_| BridgeError::CallbackDropped)?;
        if res.code != 0 {
            return Err(BridgeError::from_code(res.code));
        }
        let bytes = res
            .buffer
            .ok_or_else(|| BridgeError::InvalidPayload("OK code but null buffer".into()))?;
        String::from_utf8(bytes).map_err(|e| BridgeError::InvalidPayload(format!("utf8: {e}")))
    }
}

#[cfg(test)]
#[allow(
    // panic!() is the test signal for unexpected match arms — the
    // explicit panic on `Ok(other)` / `Err(other)` is doing real
    // work: it fails the test loudly with the unexpected variant
    // pretty-printed, which is what a test expects.
    clippy::panic,
    // The match arms below intentionally enumerate each valid
    // shape on its own line (with its own doc comment). Merging
    // them with `|` would collapse the documentation that pairs
    // each pattern with what state it represents.
    clippy::match_same_arms,
)]
mod tests {
    use super::*;

    /// On a non-Apple-Intelligence build (or non-macOS CI), the
    /// status probe should report `available: false` with a
    /// typed reason rather than erroring out — the daemon
    /// surfaces the reason to the doctor view.
    #[tokio::test]
    async fn status_returns_typed_reason_on_unavailable() {
        let r = status().await;
        // Either Ok(unavailable, reason) on hosts where the bridge
        // compiled but the model isn't ready, or Ok(available)
        // on a dev machine with Apple Intelligence enabled.
        // Hard error only when the bridge crate failed to compile.
        match r {
            Ok(StatusReport {
                available: false,
                reason: Some(_),
            }) => {}
            Ok(StatusReport {
                available: true,
                reason: None,
            }) => {}
            Ok(other) => panic!("unexpected status shape: {other:?}"),
            // BridgeUnavailable on an unbuilt host is acceptable,
            // anything else is a bug.
            Err(BridgeError::BridgeUnavailable) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// On a host where the bridge isn't compiled in, `complete`
    /// must return `BridgeUnavailable` rather than panicking.
    /// On a host where it is compiled in, we accept either a
    /// successful generation or a typed unavailability error.
    #[tokio::test]
    async fn complete_returns_typed_error_when_unavailable() {
        let r = complete("Say hi.", &GenerationOptions::new(32)).await;
        match r {
            Ok(_text) => {}
            Err(
                BridgeError::BridgeUnavailable
                | BridgeError::AppleIntelligenceDisabled
                | BridgeError::DeviceNotEligible
                | BridgeError::ModelUnavailable(_)
                | BridgeError::GenerationFailed(_),
            ) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// `complete_structured` must follow the same contract — typed
    /// error on unavailable hosts; on a built host with the model
    /// reachable, either a JSON response (caller decodes) or a
    /// typed unavailability / schema error.
    #[tokio::test]
    async fn complete_structured_returns_typed_error_when_unavailable() {
        let schema = r#"{
            "type": "object",
            "properties": { "greeting": { "type": "string" } },
            "required": ["greeting"]
        }"#;
        let r = complete_structured("Say hi as JSON.", schema, &GenerationOptions::new(64)).await;
        match r {
            Ok(_text) => {}
            Err(
                BridgeError::BridgeUnavailable
                | BridgeError::AppleIntelligenceDisabled
                | BridgeError::DeviceNotEligible
                | BridgeError::ModelUnavailable(_)
                | BridgeError::GenerationFailed(_)
                | BridgeError::SchemaParseFailure
                | BridgeError::SchemaUnsupportedShape,
            ) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn generation_options_new_omits_temperature() {
        let o = GenerationOptions::new(256);
        assert_eq!(o.max_tokens, 256);
        assert_eq!(o.temperature, None);
    }

    #[test]
    fn generation_options_with_temperature_sets_some() {
        let o = GenerationOptions::with_temperature(256, 0.0);
        assert_eq!(o.max_tokens, 256);
        // Use float-equality with explicit precision since clippy
        // doesn't object on 0.0 specifically; 0.0 is bit-exact.
        assert!(o.temperature.is_some_and(|t| t == 0.0));
    }

    #[test]
    fn generation_options_default_is_no_temperature() {
        let o = GenerationOptions::default();
        assert!(o.max_tokens > 0, "default should have some token budget");
        assert_eq!(o.temperature, None);
    }
}
