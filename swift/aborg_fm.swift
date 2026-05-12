// Swift FFI bridge for the Apple Intelligence Foundation Models
// framework. Compiled into a static library by
// `crates/foundation-models/build.rs` and linked into the
// ab_foundation_models crate.
//
// Two entry points so far:
//
//   1. `aborg_fm_status`  → reports availability of
//      `SystemLanguageModel.default`. Used by `aborg doctor llm`
//      and by extractors that fail fast when the on-device model
//      can't run.
//   2. `aborg_fm_complete` → one-shot prompt → text completion.
//      Caller passes a prompt + a soft token budget; bridge fires
//      callback once with the model's response as UTF-8.
//
// Contract (same shape as `aborg_ai.swift`):
//   - `ctx` is opaque (a boxed Rust oneshot sender); passed back
//     unmodified.
//   - Callback signature: `(ctx, *CChar, count, errCode)`.
//   - Success-with-payload: `(OK, ptr, len)`.
//   - Success-with-no-payload: `(OK, nil, 0)`.
//   - Failure: `(nonOK, nil, 0)` and a stderr message.

import Foundation
#if canImport(FoundationModels)
import FoundationModels
#endif

// MARK: - Error codes
//
// Numerically aligned with `aborg_ai.swift`'s overlapping codes
// (0=OK, 1=Generic, 8=EncodeFailure) so the Rust-side
// `BridgeError::from_code` doesn't have to special-case which
// bridge produced the code.

private let kFmOk: Int32 = 0
private let kFmGeneric: Int32 = 1
private let kFmFrameworkUnavailable: Int32 = 2
private let kFmEncodeFailure: Int32 = 8

// FM-specific codes start at 20 so they don't collide with
// anything aborg_ai uses today (which tops out at 8).
private let kFmModelUnavailable: Int32 = 20  // SDK present, model not ready
private let kFmAppleIntelligenceDisabled: Int32 = 21
private let kFmDeviceNotEligible: Int32 = 22
private let kFmPromptEmpty: Int32 = 23
private let kFmGenerationFailed: Int32 = 24

private enum AborgFmError: Error {
    case frameworkUnavailable
    case modelUnavailable(String)
    case appleIntelligenceDisabled
    case deviceNotEligible
    case promptEmpty
    case generationFailed(String)
}

private func errorCode(for err: Error) -> Int32 {
    if let e = err as? AborgFmError {
        switch e {
        case .frameworkUnavailable: return kFmFrameworkUnavailable
        case .modelUnavailable: return kFmModelUnavailable
        case .appleIntelligenceDisabled: return kFmAppleIntelligenceDisabled
        case .deviceNotEligible: return kFmDeviceNotEligible
        case .promptEmpty: return kFmPromptEmpty
        case .generationFailed: return kFmGenerationFailed
        }
    }
    return kFmGeneric
}

private func logError(_ tag: String, _ err: Error) {
    let msg = "\(tag): \(err)\n"
    FileHandle.standardError.write(msg.data(using: .utf8) ?? Data())
}

// MARK: - Availability probe

// What the doctor needs: a single JSON blob with `available: bool`
// plus a `reason` string suitable for surfacing in the CLI.
// Reasons mirror Apple's `SystemLanguageModel.Availability`
// vocabulary so the doctor can suggest the right fix
// ("Enable Apple Intelligence in System Settings", etc.).
private struct AborgFmStatus: Encodable {
    let available: Bool
    let reason: String
}

@available(macOS 26.0, *)
private func runStatus() -> AborgFmStatus {
    #if canImport(FoundationModels)
    let model = SystemLanguageModel.default
    switch model.availability {
    case .available:
        return AborgFmStatus(available: true, reason: "available")
    case .unavailable(let reason):
        // The Swift API names mirror the four documented states.
        // We keep them as machine-readable tokens; the Rust
        // doctor maps tokens to localized human-readable
        // sentences.
        switch reason {
        case .appleIntelligenceNotEnabled:
            return AborgFmStatus(available: false, reason: "apple_intelligence_not_enabled")
        case .deviceNotEligible:
            return AborgFmStatus(available: false, reason: "device_not_eligible")
        case .modelNotReady:
            return AborgFmStatus(available: false, reason: "model_not_ready")
        @unknown default:
            return AborgFmStatus(available: false, reason: "unavailable_unknown")
        }
    @unknown default:
        return AborgFmStatus(available: false, reason: "availability_unknown")
    }
    #else
    return AborgFmStatus(available: false, reason: "framework_not_built")
    #endif
}

@_cdecl("aborg_fm_status")
public func aborg_fm_status(
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let status: AborgFmStatus
    if #available(macOS 26.0, *) {
        status = runStatus()
    } else {
        status = AborgFmStatus(available: false, reason: "macos_below_26")
    }
    do {
        let data = try JSONEncoder().encode(status)
        data.withUnsafeBytes { rawBuf in
            let base = rawBuf.baseAddress?.assumingMemoryBound(to: CChar.self)
            callback(ctx, base, data.count, kFmOk)
        }
    } catch {
        logError("aborg_fm_status encode error", error)
        callback(ctx, nil, 0, kFmEncodeFailure)
    }
}

// MARK: - Supported languages
//
// What `aborg doctor` needs: the list of BCP-47 locales the
// on-device model accepts as input/output. Lets the doctor
// surface "your library_locale=ja isn't supported by Apple
// Intelligence yet" diagnostics before the user hits a runtime
// generation failure.
//
// Encoded as a JSON array of BCP-47 primary-subtag strings
// (e.g. `["en", "de", "fr", "es", "ja", "zh-Hans"]`). Apple's
// `Locale.Language.maximalIdentifier` returns the canonical
// form per language.

private struct AborgFmSupportedLanguages: Encodable {
    let locales: [String]
}

@available(macOS 26.0, *)
private func runSupportedLanguages() -> AborgFmSupportedLanguages {
    #if canImport(FoundationModels)
    let model = SystemLanguageModel.default
    let languages = Array(model.supportedLanguages)
    let locales = languages.map { $0.maximalIdentifier }.sorted()
    return AborgFmSupportedLanguages(locales: locales)
    #else
    return AborgFmSupportedLanguages(locales: [])
    #endif
}

@_cdecl("aborg_fm_supported_languages")
public func aborg_fm_supported_languages(
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let payload: AborgFmSupportedLanguages
    if #available(macOS 26.0, *) {
        payload = runSupportedLanguages()
    } else {
        payload = AborgFmSupportedLanguages(locales: [])
    }
    do {
        let data = try JSONEncoder().encode(payload)
        data.withUnsafeBytes { rawBuf in
            let base = rawBuf.baseAddress?.assumingMemoryBound(to: CChar.self)
            callback(ctx, base, data.count, kFmOk)
        }
    } catch {
        logError("aborg_fm_supported_languages encode error", error)
        callback(ctx, nil, 0, kFmEncodeFailure)
    }
}

// MARK: - Completion

// One-shot prompt. The Rust side stamps the prompt with whatever
// instructions / few-shots it needs; the bridge just runs the
// round-trip. `maxTokens` is a soft budget — the framework treats
// it as an upper bound but may stop earlier on its own EOS
// signal.
@available(macOS 26.0, *)
private func runComplete(prompt: String, maxTokens: Int) async throws -> String {
    #if canImport(FoundationModels)
    let model = SystemLanguageModel.default
    switch model.availability {
    case .available:
        break
    case .unavailable(let reason):
        switch reason {
        case .appleIntelligenceNotEnabled:
            throw AborgFmError.appleIntelligenceDisabled
        case .deviceNotEligible:
            throw AborgFmError.deviceNotEligible
        case .modelNotReady:
            throw AborgFmError.modelUnavailable("modelNotReady")
        @unknown default:
            throw AborgFmError.modelUnavailable("unknown")
        }
    @unknown default:
        throw AborgFmError.modelUnavailable("availability_unknown")
    }
    if prompt.isEmpty {
        throw AborgFmError.promptEmpty
    }
    // ── Guardrails — KNOWN GAP, see TODO below ──────────────────
    // The default `SystemLanguageModel.default` applies Apple's
    // standard content-safety guardrails. That's wrong for an
    // audiobook organiser: genre fiction routinely contains
    // violence, sex, adult themes, drug use, etc., and the
    // default guardrails will refuse to summarise / tag content
    // the framework flags.
    //
    // The entro314 reference codebase uses
    // `SystemLanguageModel(guardrails: Guardrails.developerProvided)`
    // to trust the calling app to bound the output domain (which
    // our DNA / summary extractors do via prompts and the closed
    // CacheKey vocabulary). HOWEVER, that variant does not exist
    // on our installed SDK as of macOS 26.5 / Swift 6.3.2 —
    // `SystemLanguageModel.Guardrails` only exposes `.default`
    // here. Both `.permissive` and `.developerProvided` are
    // unresolved at compile time.
    //
    // TODO(C5.7-followup): revisit on the next SDK update. When
    // a less-restrictive variant ships, swap in:
    //
    //     let model = SystemLanguageModel(guardrails: .developerProvided)
    //     let session = LanguageModelSession(model: model)
    //
    // For now, stick with the default-guardrails session so the
    // bridge compiles. Affected stages (DNA, summary, story
    // arc, characters) should detect refusal-style outputs and
    // surface a typed BridgeError::GenerationFailed("guardrails")
    // for the Rust side to log + skip.
    let session = LanguageModelSession()
    let options = GenerationOptions(maximumResponseTokens: max(1, maxTokens))
    do {
        let response = try await session.respond(to: prompt, options: options)
        return response.content
    } catch {
        throw AborgFmError.generationFailed("\(error)")
    }
    #else
    _ = prompt
    _ = maxTokens
    throw AborgFmError.frameworkUnavailable
    #endif
}

@_cdecl("aborg_fm_complete")
public func aborg_fm_complete(
    _ prompt: UnsafePointer<CChar>?,
    _ maxTokens: Int,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let promptStr = prompt.flatMap { String(validatingCString: $0) } ?? ""
    Task.detached {
        do {
            let text: String
            if #available(macOS 26.0, *) {
                text = try await runComplete(prompt: promptStr, maxTokens: maxTokens)
            } else {
                throw AborgFmError.frameworkUnavailable
            }
            let data = text.data(using: .utf8) ?? Data()
            data.withUnsafeBytes { rawBuf in
                let base = rawBuf.baseAddress?.assumingMemoryBound(to: CChar.self)
                callback(ctx, base, data.count, kFmOk)
            }
        } catch {
            logError("aborg_fm_complete", error)
            callback(ctx, nil, 0, errorCode(for: error))
        }
    }
}
