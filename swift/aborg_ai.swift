// Swift FFI bridge for the ABorganizer AI features.
//
// Currently exposes one entry point: `aborg_transcribe_window`,
// callable from Rust via the static lib produced by
// `crates/transcript/build.rs`.
//
// As of slice 3A.3 the body uses `Speech.SpeechAnalyzer` +
// `Speech.SpeechTranscriber` (macOS 26's Apple-Intelligence-aware
// replacement for `SFSpeechRecognizer`). Audio is read via
// `AVAudioFile`, sliced in PCM space to the requested window, and
// converted to the engine's preferred format via
// `AVAudioConverter` when needed.
//
// Contract:
//   - Rust passes `(input_path, start_secs, end_secs, locale, ctx,
//     callback)`.
//   - Swift fires the C callback exactly once. Success →
//     UTF-8 JSON segment array, length non-zero. Failure → null
//     pointer + length zero. Error detail goes to stderr.
//   - `ctx` is opaque (a boxed Rust oneshot sender); passed back
//     unmodified.
//
// JSON shape (matches `bridge::TranscriptSegment`):
//   [{"start_ms": u64, "end_ms": u64, "text": String, "confidence": f32}, ...]
//
// Timestamps are absolute file-time (CMTime seconds × 1000), so a
// transcript window starting at 60 s yields segments whose
// `start_ms >= 60_000` — matches the contract documented on the
// Rust side.

import Foundation
import AVFoundation
import Speech

// MARK: - JSON output shape

private struct AborgSegment: Codable {
    let start_ms: UInt64
    let end_ms: UInt64
    let text: String
    let confidence: Float
}

// MARK: - Helpers

@available(macOS 26.0, *)
private func cmTimeToMs(_ t: CMTime) -> UInt64 {
    guard t.isValid, t.timescale > 0 else { return 0 }
    let s = t.seconds
    guard s.isFinite, s >= 0 else { return 0 }
    return UInt64(s * 1000.0)
}

/// Mean confidence across the AttributedString's runs that carry
/// the `transcriptionConfidence` attribute. Returns 0 when the
/// engine didn't attach any (e.g. caller didn't request the
/// attribute). Caller asked for `.transcriptionConfidence` in
/// `attributeOptions`, so the runs should be present.
@available(macOS 26.0, *)
private func meanConfidence(_ attr: AttributedString) -> Float {
    var sum: Double = 0
    var count: Double = 0
    for run in attr.runs {
        if let c = run.transcriptionConfidence {
            sum += c
            count += 1
        }
    }
    return count > 0 ? Float(sum / count) : 0
}

// MARK: - Transcription pipeline

@available(macOS 26.0, *)
private enum AborgAIError: Error {
    case frameworkUnavailable
    case localeUnsupported(String)
    case modelNotInstalled(String)
    case noCompatibleAudioFormat
    case windowEmpty
    case readFailure(String)
}

@available(macOS 26.0, *)
private func runTranscribe(
    pathStr: String,
    startSecs: Double,
    endSecs: Double,
    localeStr: String
) async throws -> [AborgSegment] {
    // 1. Framework availability gate. If the user is on a
    //    machine where Apple Intelligence isn't enabled,
    //    `isAvailable` returns false; the daemon-level probe
    //    catches this earlier, but defensive here.
    guard SpeechTranscriber.isAvailable else {
        throw AborgAIError.frameworkUnavailable
    }

    // 2. Resolve the locale via the transcriber's own
    //    equivalence map (`en` → `en-US`, etc.). If nothing
    //    matches we can't proceed.
    let requested = Locale(identifier: localeStr)
    guard let supported = await SpeechTranscriber.supportedLocale(equivalentTo: requested) else {
        throw AborgAIError.localeUnsupported(localeStr)
    }

    // 3. Build the transcriber. Time-range + confidence
    //    attributes are what the segment array needs; volatile
    //    results + alternatives are not requested (we want
    //    finalised text only).
    let transcriber = SpeechTranscriber(
        locale: supported,
        transcriptionOptions: [],
        reportingOptions: [],
        attributeOptions: [.audioTimeRange, .transcriptionConfidence]
    )
    let modules: [any SpeechModule] = [transcriber]

    // 4. Model installation gate. `AssetInventory` reports
    //    `installed | downloading | supported | unsupported`.
    //    Only `.installed` lets us proceed without download —
    //    download is a future slice (needs UX + tunable).
    let status = await AssetInventory.status(forModules: modules)
    if status != .installed {
        throw AborgAIError.modelNotInstalled(
            "locale=\(supported.identifier) status=\(status)"
        )
    }

    // 5. Engine audio format.
    guard let engineFormat = await SpeechAnalyzer
        .bestAvailableAudioFormat(compatibleWith: modules) else {
        throw AborgAIError.noCompatibleAudioFormat
    }

    // 6. Read the requested window from the file, in the file's
    //    native processing format, then convert to engine
    //    format. Slicing in PCM space is bounded — a 6-min
    //    16kHz mono Float32 buffer is ~12 MB. For the
    //    future full-book stage we'll switch to chunked
    //    AVAssetReader to keep RAM bounded.
    let url = URL(fileURLWithPath: pathStr)
    let file = try AVAudioFile(forReading: url)
    let nativeFormat = file.processingFormat
    let nativeSampleRate = nativeFormat.sampleRate
    let totalFrames = file.length

    let startFrame = AVAudioFramePosition(max(0.0, startSecs * nativeSampleRate))
    let endFrameRaw = AVAudioFramePosition(endSecs * nativeSampleRate)
    let endFrame = min(endFrameRaw, totalFrames)
    if endFrame <= startFrame {
        throw AborgAIError.windowEmpty
    }
    let frameCount = AVAudioFrameCount(endFrame - startFrame)
    file.framePosition = startFrame

    guard let nativeBuffer = AVAudioPCMBuffer(
        pcmFormat: nativeFormat, frameCapacity: frameCount
    ) else {
        throw AborgAIError.readFailure("alloc native PCM buffer")
    }
    do {
        try file.read(into: nativeBuffer, frameCount: frameCount)
    } catch {
        throw AborgAIError.readFailure("file.read: \(error)")
    }

    let analyzerBuffer: AVAudioPCMBuffer
    if nativeFormat.isEqual(engineFormat) {
        analyzerBuffer = nativeBuffer
    } else {
        guard let converter = AVAudioConverter(from: nativeFormat, to: engineFormat) else {
            throw AborgAIError.readFailure("AVAudioConverter init failed")
        }
        // Output capacity: scale by sample-rate ratio + slack
        // for resampling lookahead.
        let ratio = engineFormat.sampleRate / nativeFormat.sampleRate
        let outCap = AVAudioFrameCount(Double(nativeBuffer.frameLength) * ratio) + 512
        guard let outBuffer = AVAudioPCMBuffer(
            pcmFormat: engineFormat, frameCapacity: outCap
        ) else {
            throw AborgAIError.readFailure("alloc engine PCM buffer")
        }
        var hasFed = false
        var convertError: NSError?
        let convStatus = converter.convert(to: outBuffer, error: &convertError) {
            _, status in
            if hasFed {
                status.pointee = .endOfStream
                return nil
            }
            hasFed = true
            status.pointee = .haveData
            return nativeBuffer
        }
        if convStatus == .error {
            throw AborgAIError.readFailure(
                "AVAudioConverter.convert: \(convertError?.localizedDescription ?? "unknown")"
            )
        }
        analyzerBuffer = outBuffer
    }

    // 7. Build the input AsyncSequence with one buffer at the
    //    window's absolute start time, then finish. The
    //    transcriber's results will carry `range` values in
    //    that same absolute time-base.
    let windowStart = CMTime(
        seconds: startSecs, preferredTimescale: 1_000_000
    )
    let (inputs, continuation) = AsyncStream.makeStream(of: AnalyzerInput.self)
    continuation.yield(AnalyzerInput(buffer: analyzerBuffer, bufferStartTime: windowStart))
    continuation.finish()

    // 8. Drain results concurrently with feeding the analyzer.
    let resultsTask = Task { () throws -> [AborgSegment] in
        var collected: [AborgSegment] = []
        for try await result in transcriber.results {
            let textVal = String(result.text.characters)
            let trimmed = textVal.trimmingCharacters(in: .whitespacesAndNewlines)
            if trimmed.isEmpty { continue }
            collected.append(AborgSegment(
                start_ms: cmTimeToMs(result.range.start),
                end_ms: cmTimeToMs(result.range.end),
                text: trimmed,
                confidence: meanConfidence(result.text)
            ))
        }
        return collected
    }

    // 9. Run the analyzer end-to-end.
    let analyzer = SpeechAnalyzer(modules: modules)
    try await analyzer.prepareToAnalyze(in: engineFormat)
    try await analyzer.start(inputSequence: inputs)
    try await analyzer.finalizeAndFinishThroughEndOfInput()

    return try await resultsTask.value
}

// MARK: - C entry point

@_cdecl("aborg_transcribe_window")
public func aborg_transcribe_window(
    _ inputPath: UnsafePointer<CChar>?,
    _ startSecs: Double,
    _ endSecs: Double,
    _ locale: UnsafePointer<CChar>?,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int) -> Void
) {
    let pathStr = inputPath.flatMap { String(validatingCString: $0) } ?? ""
    let localeStr = locale.flatMap { String(validatingCString: $0) } ?? "en-US"

    guard #available(macOS 26.0, *) else {
        // Build script targets macOS 26.0, so unreachable when
        // built normally. Defensive call back with null on the
        // off chance the dylib is loaded on an older host.
        callback(ctx, nil, 0)
        return
    }

    // Detached task — the C entry point returns immediately;
    // Swift's runtime keeps the Task alive until completion.
    // The Rust side awaits the callback via a oneshot.
    Task {
        do {
            let segments = try await runTranscribe(
                pathStr: pathStr,
                startSecs: startSecs,
                endSecs: endSecs,
                localeStr: localeStr
            )
            let encoder = JSONEncoder()
            encoder.outputFormatting = []
            let data = try encoder.encode(segments)
            data.withUnsafeBytes { rawBuf in
                let base = rawBuf.baseAddress?.assumingMemoryBound(to: CChar.self)
                callback(ctx, base, data.count)
            }
        } catch {
            let msg = "aborg_transcribe_window error: \(error)\n"
            FileHandle.standardError.write(msg.data(using: .utf8) ?? Data())
            callback(ctx, nil, 0)
        }
    }
}
