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
// Slice C5.7.c additions (structured generation).
private let kFmSchemaParseFailure: Int32 = 25     // input JSON Schema invalid
private let kFmSchemaUnsupportedShape: Int32 = 26 // valid JSON but a shape we don't map

private enum AborgFmError: Error {
    case frameworkUnavailable
    case modelUnavailable(String)
    case appleIntelligenceDisabled
    case deviceNotEligible
    case promptEmpty
    case generationFailed(String)
    case schemaParseFailure(String)
    case schemaUnsupportedShape(String)
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
        case .schemaParseFailure: return kFmSchemaParseFailure
        case .schemaUnsupportedShape: return kFmSchemaUnsupportedShape
        }
    }
    return kFmGeneric
}

private func logError(_ tag: String, _ err: Error) {
    let msg = "\(tag): \(err)\n"
    FileHandle.standardError.write(msg.data(using: .utf8) ?? Data())
}

// MARK: - Optional Swift-side debug logs
//
// Set `ABORG_FM_SWIFT_DEBUG_LOGS=1` (or any non-empty value)
// in the daemon's environment to get verbose diagnostics from
// inside the bridge: prompts received, generation options
// constructed, response sizes, structured-output schema parse
// breadcrumbs. Off by default — production daemon stays quiet,
// stderr only carries the `logError` lines on actual failure.
//
// Why a Swift env var instead of routing through Rust tracing:
// the Rust tracing layer can't see into the Task that runs
// inside `runComplete()` after `Task.detached` jumps. Without a
// Swift-local print we go blind whenever the model returns
// malformed JSON, the schema rejects a token, etc. The cost
// of the helper is one ProcessInfo lookup per call when the
// var is unset (cached below).

private let aborgFmDebugLogsEnabled: Bool = {
    let env = ProcessInfo.processInfo.environment["ABORG_FM_SWIFT_DEBUG_LOGS"]
    return env != nil && !(env?.isEmpty ?? true)
}()

private func debugLog(_ tag: String, _ msg: @autoclosure () -> String) {
    guard aborgFmDebugLogsEnabled else { return }
    let line = "[aborg_fm.debug] \(tag): \(msg())\n"
    FileHandle.standardError.write(line.data(using: .utf8) ?? Data())
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
private func runComplete(prompt: String, maxTokens: Int, temperature: Double?) async throws -> String {
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
    // ── Guardrails — Apple has not exposed a public knob ─────────
    // The default `SystemLanguageModel.default` applies Apple's
    // standard content-safety guardrails. For an audiobook
    // organiser this is suboptimal: genre fiction routinely
    // contains violence, sex, adult themes, drug use, etc., and
    // the default guardrails may refuse to summarise / tag
    // content the framework flags.
    //
    // We surveyed the entro314-labs/tauri-apple-intelligence
    // reference (`apple-ai.swift`) which appears to expose a
    // `Guardrails.developerProvided` knob. That turned out to be
    // a private-memory mutation, NOT a public API:
    //
    //     struct Guardrails {     // <-- entro314's OWN struct
    //       static var developerProvided: SystemLanguageModel.Guardrails {
    //         var guardrails = SystemLanguageModel.Guardrails.default
    //         withUnsafeMutablePointer(to: &guardrails) { ptr in
    //           let rawPtr = UnsafeMutableRawPointer(ptr)
    //           let boolPtr = rawPtr.assumingMemoryBound(to: Bool.self)
    //           boolPtr.pointee = false   // flips a private "strict" flag
    //         }
    //         return guardrails
    //       }
    //     }
    //
    // i.e. they cast `SystemLanguageModel.Guardrails.default`'s
    // memory to a `Bool*` and write `false` at byte 0 to flip
    // what looks like a private "strict" flag. We REJECT this
    // approach: it depends on private memory layout, breaks
    // silently on any SDK minor update, and would not survive
    // notarisation review.
    //
    // The public `SystemLanguageModel.Guardrails` type as of
    // macOS 26.5 / Swift 6.3.2 only exposes `.default`. Until
    // Apple ships a documented developer-customisable Guardrails
    // surface, we stick with `.default` here. Affected stages
    // (DNA, summary, story arc, characters) should detect
    // refusal-style outputs and surface a typed
    // `BridgeError::GenerationFailed("guardrails")` for the Rust
    // side to log + skip. Re-evaluate on each macOS / Xcode
    // release.
    let session = LanguageModelSession()
    let clampedMaxTokens = max(1, maxTokens)
    let options: GenerationOptions
    if let t = temperature {
        options = GenerationOptions(
            temperature: t,
            maximumResponseTokens: clampedMaxTokens
        )
        debugLog(
            "runComplete",
            "max_tokens=\(clampedMaxTokens) temperature=\(t) prompt_len=\(prompt.count)"
        )
    } else {
        options = GenerationOptions(maximumResponseTokens: clampedMaxTokens)
        debugLog(
            "runComplete",
            "max_tokens=\(clampedMaxTokens) temperature=default prompt_len=\(prompt.count)"
        )
    }
    do {
        let response = try await session.respond(to: prompt, options: options)
        debugLog("runComplete", "response_len=\(response.content.count)")
        return response.content
    } catch {
        throw AborgFmError.generationFailed("\(error)")
    }
    #else
    _ = (prompt, maxTokens, temperature)
    throw AborgFmError.frameworkUnavailable
    #endif
}

@_cdecl("aborg_fm_complete")
public func aborg_fm_complete(
    _ prompt: UnsafePointer<CChar>?,
    _ maxTokens: Int,
    _ temperature: Double,
    _ useTemperature: Int32,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let promptStr = prompt.flatMap { String(validatingCString: $0) } ?? ""
    // Rust packs `Option<f64>` into the (Double, Int32) pair:
    // `useTemperature != 0` means "use the value"; `== 0` means
    // "keep framework default and ignore `temperature`."
    let tempOpt: Double? = useTemperature != 0 ? temperature : nil
    Task.detached {
        do {
            let text: String
            if #available(macOS 26.0, *) {
                text = try await runComplete(
                    prompt: promptStr,
                    maxTokens: maxTokens,
                    temperature: tempOpt
                )
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

// MARK: - Structured completion (schema-constrained)
//
// `session.respond(to:, schema:, includeSchemaInPrompt: true,
// options:)` constrains the model at decode time to produce
// output that round-trips through the supplied
// `GenerationSchema`. The Rust side passes a JSON Schema
// fragment naming the shape it wants; this entry parses it into
// a `DynamicGenerationSchema`, runs the round-trip, and returns
// the structured output as a JSON string via
// `GeneratedContent.jsonString`.
//
// Shape support (slice C5.7.c initial pass):
//   * object with `properties` + optional `required`
//   * primitives: `string`, `integer`, `number`, `boolean`
//   * arrays of the above
// Shapes not yet supported: `oneOf`, `$ref`, nested arrays of
// arrays, enums. Adding any is a small append below.
//
// Rationale: every LLM extractor (DNA, summary, story arc,
// characters) currently asks the model for JSON in prompts and
// parses with `serde_json::from_str` on the Rust side, with
// known reliability quirks (DNA stage has a test for "the
// model occasionally omits empty arrays"). Schema-constrained
// generation moves that contract from prompt-and-pray to
// decode-time guarantee.

@available(macOS 26.0, *)
private func buildDynamicSchema(
    name: String,
    json: [String: Any]
) throws -> DynamicGenerationSchema {
    if let typeAny = json["type"] as? String {
        switch typeAny {
        case "object":
            let propertiesDict = json["properties"] as? [String: Any] ?? [:]
            let requiredArr = json["required"] as? [String] ?? []
            let requiredSet = Set(requiredArr)
            var properties: [DynamicGenerationSchema.Property] = []
            for (propName, propAny) in propertiesDict {
                guard let propJson = propAny as? [String: Any] else {
                    throw AborgFmError.schemaUnsupportedShape(
                        "property '\(propName)' is not an object")
                }
                let childName = "\(name)_\(propName)"
                let childSchema = try buildDynamicSchema(name: childName, json: propJson)
                let isOptional = !requiredSet.contains(propName)
                properties.append(
                    DynamicGenerationSchema.Property(
                        name: propName,
                        schema: childSchema,
                        isOptional: isOptional
                    )
                )
            }
            return DynamicGenerationSchema(name: name, properties: properties)
        case "string":
            return DynamicGenerationSchema(type: String.self)
        case "integer":
            return DynamicGenerationSchema(type: Int.self)
        case "number":
            return DynamicGenerationSchema(type: Double.self)
        case "boolean":
            return DynamicGenerationSchema(type: Bool.self)
        case "array":
            guard let itemsJson = json["items"] as? [String: Any] else {
                throw AborgFmError.schemaUnsupportedShape(
                    "array '\(name)' missing 'items' object")
            }
            let itemSchema = try buildDynamicSchema(
                name: "\(name)_item", json: itemsJson)
            return DynamicGenerationSchema(arrayOf: itemSchema)
        default:
            throw AborgFmError.schemaUnsupportedShape(
                "unsupported JSON Schema 'type': \(typeAny)")
        }
    }
    throw AborgFmError.schemaUnsupportedShape(
        "schema node missing 'type' (oneOf / $ref not yet supported)")
}

@available(macOS 26.0, *)
private func runCompleteStructured(
    prompt: String,
    schemaJsonStr: String,
    maxTokens: Int,
    temperature: Double?
) async throws -> String {
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

    // Parse the input JSON Schema string.
    guard let schemaData = schemaJsonStr.data(using: .utf8) else {
        throw AborgFmError.schemaParseFailure("non-utf8 schema input")
    }
    let parsed: Any
    do {
        parsed = try JSONSerialization.jsonObject(with: schemaData, options: [])
    } catch {
        throw AborgFmError.schemaParseFailure("\(error)")
    }
    guard let schemaObj = parsed as? [String: Any] else {
        throw AborgFmError.schemaParseFailure("root must be a JSON object")
    }

    let rootSchema = try buildDynamicSchema(name: "Root", json: schemaObj)
    let generationSchema: GenerationSchema
    do {
        generationSchema = try GenerationSchema(root: rootSchema, dependencies: [])
    } catch {
        throw AborgFmError.schemaUnsupportedShape("GenerationSchema build: \(error)")
    }

    // Guardrails: stuck with `.default` until Apple ships a public
    // developer-customisable Guardrails surface on
    // `SystemLanguageModel`. See the long comment in
    // `runComplete()` for the diagnostic + the reason we won't
    // adopt the entro314-labs private-memory hack.
    let session = LanguageModelSession()
    let clampedMaxTokens = max(1, maxTokens)
    let options: GenerationOptions
    if let t = temperature {
        options = GenerationOptions(
            temperature: t,
            maximumResponseTokens: clampedMaxTokens
        )
        debugLog(
            "runCompleteStructured",
            "max_tokens=\(clampedMaxTokens) temperature=\(t) prompt_len=\(prompt.count) schema_len=\(schemaJsonStr.count)"
        )
    } else {
        options = GenerationOptions(maximumResponseTokens: clampedMaxTokens)
        debugLog(
            "runCompleteStructured",
            "max_tokens=\(clampedMaxTokens) temperature=default prompt_len=\(prompt.count) schema_len=\(schemaJsonStr.count)"
        )
    }
    do {
        let response = try await session.respond(
            to: prompt,
            schema: generationSchema,
            includeSchemaInPrompt: true,
            options: options
        )
        let json = response.content.jsonString
        debugLog("runCompleteStructured", "response_json_len=\(json.count)")
        return json
    } catch {
        throw AborgFmError.generationFailed("\(error)")
    }
    #else
    _ = (prompt, schemaJsonStr, maxTokens, temperature)
    throw AborgFmError.frameworkUnavailable
    #endif
}

@_cdecl("aborg_fm_complete_structured")
public func aborg_fm_complete_structured(
    _ prompt: UnsafePointer<CChar>?,
    _ schemaJson: UnsafePointer<CChar>?,
    _ maxTokens: Int,
    _ temperature: Double,
    _ useTemperature: Int32,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let promptStr = prompt.flatMap { String(validatingCString: $0) } ?? ""
    let schemaStr = schemaJson.flatMap { String(validatingCString: $0) } ?? ""
    let tempOpt: Double? = useTemperature != 0 ? temperature : nil
    Task.detached {
        do {
            let text: String
            if #available(macOS 26.0, *) {
                text = try await runCompleteStructured(
                    prompt: promptStr,
                    schemaJsonStr: schemaStr,
                    maxTokens: maxTokens,
                    temperature: tempOpt
                )
            } else {
                throw AborgFmError.frameworkUnavailable
            }
            let data = text.data(using: .utf8) ?? Data()
            data.withUnsafeBytes { rawBuf in
                let base = rawBuf.baseAddress?.assumingMemoryBound(to: CChar.self)
                callback(ctx, base, data.count, kFmOk)
            }
        } catch {
            logError("aborg_fm_complete_structured", error)
            callback(ctx, nil, 0, errorCode(for: error))
        }
    }
}

// MARK: - Streaming completion (AI-2)
//
// Apple's `LanguageModelSession.streamResponse(to:options:)`
// returns an `AsyncSequence` of incremental snapshot strings.
// Each yielded value is the FULL response so far (cumulative
// snapshot), not the delta — so for delta-style chunks we
// compute the suffix vs. the previous emission. That's the
// contract the Rust side wants: one `Ok(chunk)` per token-y
// fragment, then EOS.
//
// FFI contract (mirror of `extern fn aborg_fm_complete_stream`
// in the Rust side):
//
//   - **Chunk**: callback fired with `(ptr, len > 0, code = 0)`
//                for each new fragment.
//   - **EOS**:   callback fired with `(ptr = nil, len = 0, code = 0)`.
//   - **Error**: callback fired with `(ptr = nil, len = 0, code != 0)`
//                — no further callbacks, no EOS.
//
// Always exactly one terminal callback (EOS or error). The
// Rust side reclaims its boxed Sender on whichever terminal
// arrives.

@_cdecl("aborg_fm_complete_stream")
public func aborg_fm_complete_stream(
    _ prompt: UnsafePointer<CChar>?,
    _ maxTokens: Int,
    _ temperature: Double,
    _ useTemperature: Int32,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let promptStr = prompt.flatMap { String(validatingCString: $0) } ?? ""
    let tempOpt: Double? = useTemperature != 0 ? temperature : nil
    Task.detached {
        do {
            if #available(macOS 26.0, *) {
                try await runCompleteStream(
                    prompt: promptStr,
                    maxTokens: maxTokens,
                    temperature: tempOpt,
                    onChunk: { chunk in
                        let data = chunk.data(using: .utf8) ?? Data()
                        data.withUnsafeBytes { rawBuf in
                            let base = rawBuf.baseAddress?.assumingMemoryBound(to: CChar.self)
                            callback(ctx, base, data.count, kFmOk)
                        }
                    }
                )
            } else {
                throw AborgFmError.frameworkUnavailable
            }
            // EOS terminal.
            callback(ctx, nil, 0, kFmOk)
        } catch {
            logError("aborg_fm_complete_stream", error)
            callback(ctx, nil, 0, errorCode(for: error))
        }
    }
}

@available(macOS 26.0, *)
private func runCompleteStream(
    prompt: String,
    maxTokens: Int,
    temperature: Double?,
    onChunk: (String) -> Void
) async throws {
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
    // Guardrails: same `.default`-only constraint as
    // `runComplete()`; see the long comment there for the
    // rationale + the reason we won't take the entro314 hack.
    let session = LanguageModelSession()
    let clampedMaxTokens = max(1, maxTokens)
    let options: GenerationOptions
    if let t = temperature {
        options = GenerationOptions(
            temperature: t,
            maximumResponseTokens: clampedMaxTokens
        )
        debugLog(
            "runCompleteStream",
            "max_tokens=\(clampedMaxTokens) temperature=\(t) prompt_len=\(prompt.count)"
        )
    } else {
        options = GenerationOptions(maximumResponseTokens: clampedMaxTokens)
        debugLog(
            "runCompleteStream",
            "max_tokens=\(clampedMaxTokens) temperature=default prompt_len=\(prompt.count)"
        )
    }
    // Apple's `streamResponse` yields cumulative snapshots —
    // each value is the full response so far. To deliver
    // *deltas* over the FFI we keep a running `previous` and
    // emit only the suffix that's new.
    var previous = ""
    do {
        let stream = session.streamResponse(to: prompt, options: options)
        for try await snapshot in stream {
            // `snapshot.content` is the cumulative String for
            // unstructured stream responses on macOS 26+.
            let full: String = snapshot.content
            if full.count > previous.count, full.hasPrefix(previous) {
                let delta = String(full.dropFirst(previous.count))
                if !delta.isEmpty {
                    onChunk(delta)
                }
                previous = full
            } else if full != previous {
                // Non-monotone snapshot (rare; framework resets
                // mid-stream). Emit the whole snapshot as a
                // single chunk — the Rust side concatenates.
                onChunk(full)
                previous = full
            }
        }
        debugLog("runCompleteStream", "stream_done total_len=\(previous.count)")
    } catch {
        throw AborgFmError.generationFailed("\(error)")
    }
    #else
    _ = (prompt, maxTokens, temperature, onChunk)
    throw AborgFmError.frameworkUnavailable
    #endif
}
