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
import NaturalLanguage
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

/// Wrap a `CMSampleBuffer`'s audio data in a fresh
/// `AVAudioPCMBuffer` matching `format`.
///
/// Returns `nil` on any of the well-known failure paths
/// (incomplete sample, missing block buffer, byte-count
/// mismatch). Copies the bytes (not `bufferListNoCopy`) so the
/// resulting PCM buffer is independent of the
/// `CMSampleBuffer`'s lifetime — required because the analyzer
/// stream yields multiple inputs that mustn't share / overwrite
/// each other's backing memory.
///
/// Format-agnostic: writes into the PCM buffer's first audio
/// buffer regardless of whether the layout is Int16, Float32,
/// or anything else. The caller is responsible for matching
/// the AVAssetReader output settings to `format` so the byte
/// counts line up.
@available(macOS 26.0, *)
private func makePcmBuffer(
    sample: CMSampleBuffer,
    format: AVAudioFormat
) -> AVAudioPCMBuffer? {
    guard CMSampleBufferDataIsReady(sample) else { return nil }
    let sampleCount = CMSampleBufferGetNumSamples(sample)
    if sampleCount <= 0 { return nil }
    let numFrames = AVAudioFrameCount(sampleCount)

    guard let pcm = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: numFrames) else {
        return nil
    }
    pcm.frameLength = numFrames

    guard let dataBuffer = CMSampleBufferGetDataBuffer(sample) else {
        return nil
    }
    var lengthAtOffset = 0
    var totalLength = 0
    var dataPointer: UnsafeMutablePointer<CChar>?
    let status = CMBlockBufferGetDataPointer(
        dataBuffer,
        atOffset: 0,
        lengthAtOffsetOut: &lengthAtOffset,
        totalLengthOut: &totalLength,
        dataPointerOut: &dataPointer
    )
    guard status == noErr, let src = dataPointer else { return nil }

    let bytesPerFrame = Int(format.streamDescription.pointee.mBytesPerFrame)
    let byteCount = Int(numFrames) * bytesPerFrame
    guard byteCount > 0, byteCount <= totalLength else { return nil }

    // Format-agnostic byte copy via the PCMBuffer's underlying
    // AudioBufferList. Avoids dispatching on commonFormat
    // (Int16 / Float32 / Int32 each need a different typed
    // channelData accessor).
    let abl = pcm.mutableAudioBufferList
    guard let dst = abl.pointee.mBuffers.mData else { return nil }
    let dstCapacity = Int(abl.pointee.mBuffers.mDataByteSize)
    guard byteCount <= dstCapacity else { return nil }
    memcpy(dst, src, byteCount)
    return pcm
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

// MARK: - C ABI error codes

// Stable numeric codes passed back through the C callback as the
// fourth argument. The Rust side maps each onto a typed
// `BridgeError` variant — no string-matching. Order matters: new
// codes append, never reuse a number.
//
// 0 is success (callback fired with a real payload). Any other
// value means the data pointer + length are unspecified
// (typically null + 0).
private let kErrCodeOK: Int32 = 0
private let kErrCodeGeneric: Int32 = 1
private let kErrCodeFrameworkUnavailable: Int32 = 2
private let kErrCodeLocaleUnsupported: Int32 = 3
private let kErrCodeModelNotInstalled: Int32 = 4
private let kErrCodeWindowEmpty: Int32 = 5
private let kErrCodeNoCompatibleAudioFormat: Int32 = 6
private let kErrCodeReadFailure: Int32 = 7
private let kErrCodeEncodeFailure: Int32 = 8

@available(macOS 26.0, *)
private func errorCode(_ err: Error) -> Int32 {
    if let e = err as? AborgAIError {
        switch e {
        case .frameworkUnavailable: return kErrCodeFrameworkUnavailable
        case .localeUnsupported: return kErrCodeLocaleUnsupported
        case .modelNotInstalled: return kErrCodeModelNotInstalled
        case .noCompatibleAudioFormat: return kErrCodeNoCompatibleAudioFormat
        case .windowEmpty: return kErrCodeWindowEmpty
        case .readFailure: return kErrCodeReadFailure
        }
    }
    return kErrCodeGeneric
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

    // 6. Open via AVAssetReader and stream the requested window
    //    to the analyzer as a sequence of small PCM buffers.
    //    AVAssetReader does decode + windowing (via timeRange)
    //    + format conversion (via outputSettings) in one
    //    pipeline. The analyzer sees continuous audio across
    //    the whole window — no chunk-boundary artifacts that
    //    the previous Rust-side chunking approach risked.
    let url = URL(fileURLWithPath: pathStr)
    let asset = AVURLAsset(url: url)
    let audioTracks: [AVAssetTrack]
    do {
        audioTracks = try await asset.loadTracks(withMediaType: .audio)
    } catch {
        throw AborgAIError.readFailure("loadTracks: \(error)")
    }
    guard let audioTrack = audioTracks.first else {
        throw AborgAIError.readFailure("no audio track in asset")
    }
    let reader: AVAssetReader
    do {
        reader = try AVAssetReader(asset: asset)
    } catch {
        throw AborgAIError.readFailure("AVAssetReader init: \(error)")
    }

    // Window via timeRange. CMTimeRange handles the slicing
    // server-side so we don't have to count frames.
    let startTime = CMTime(seconds: startSecs, preferredTimescale: 1_000_000)
    let endTime = CMTime(seconds: endSecs, preferredTimescale: 1_000_000)
    let duration = CMTimeSubtract(endTime, startTime)
    if CMTimeCompare(duration, .zero) <= 0 {
        throw AborgAIError.windowEmpty
    }
    reader.timeRange = CMTimeRange(start: startTime, duration: duration)

    // Ask the reader to deliver PCM matching the engine's
    // expected format exactly — sample rate, channels, bit
    // depth, float/int, byte order, interleaving. AVAssetReader
    // does the resampling + channel-collapse + format conversion
    // in one pass; mirroring the format end-to-end means
    // makePcmBuffer can do a plain memcpy without per-format
    // branching, and the analyzer never has to reject a chunk
    // for layout mismatch. macOS 26's `bestAvailableAudioFormat`
    // returns Int16 mono 16 kHz at the time of writing — but
    // the format-derived settings track whatever Apple changes
    // it to.
    let asbd = engineFormat.streamDescription.pointee
    let isFloat = (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0
    let isBigEndian = (asbd.mFormatFlags & kAudioFormatFlagIsBigEndian) != 0
    let isNonInterleaved = (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0
    let outputSettings: [String: Any] = [
        AVFormatIDKey: kAudioFormatLinearPCM,
        AVSampleRateKey: asbd.mSampleRate,
        AVNumberOfChannelsKey: Int(asbd.mChannelsPerFrame),
        AVLinearPCMBitDepthKey: Int(asbd.mBitsPerChannel),
        AVLinearPCMIsFloatKey: isFloat,
        AVLinearPCMIsBigEndianKey: isBigEndian,
        AVLinearPCMIsNonInterleaved: isNonInterleaved,
    ]
    let trackOutput = AVAssetReaderTrackOutput(track: audioTrack, outputSettings: outputSettings)
    trackOutput.alwaysCopiesSampleData = true
    guard reader.canAdd(trackOutput) else {
        throw AborgAIError.readFailure("reader can't add track output")
    }
    reader.add(trackOutput)
    if !reader.startReading() {
        let detail = reader.error.map { "\($0)" } ?? "unknown"
        throw AborgAIError.readFailure("AVAssetReader.startReading: \(detail)")
    }

    // 7. Bridge CMSampleBuffer → AVAudioPCMBuffer → AnalyzerInput
    //    one chunk at a time. Each CMSampleBuffer is typically
    //    a few thousand frames (~100 ms at 16 kHz); we forward
    //    them as-is so the analyzer's internal buffering is the
    //    only place audio piles up.
    let (inputs, continuation) = AsyncStream.makeStream(of: AnalyzerInput.self)

    // Spawn the producer task so feeding overlaps with
    // transcriber result drain (see resultsTask below).
    let producer = Task {
        defer { continuation.finish() }
        while !Task.isCancelled {
            guard let sample = trackOutput.copyNextSampleBuffer() else {
                return
            }
            guard let pcm = makePcmBuffer(sample: sample, format: engineFormat) else {
                // Skip the chunk on conversion failure;
                // aggregate-level errors surface via
                // reader.status when the producer ends.
                continue
            }
            let pts = CMSampleBufferGetPresentationTimeStamp(sample)
            continuation.yield(AnalyzerInput(buffer: pcm, bufferStartTime: pts))
        }
    }
    _ = producer  // hold the task alive; the defer fires on completion

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

// MARK: - Language detection

// `NLLanguageRecognizer` runs on text only — feed it concatenated
// tag fields (pre-transcribe) or transcript segments past the
// jingle (post-transcribe). Returns ISO-639-1 (or BCP-47 for
// scripts) code + confidence + top-N alternatives.
//
// Empty / whitespace-only input → null payload (Rust side maps
// to `None`). Real failures are out-of-memory only here; the
// framework can't really error on string processing.

private struct AborgLanguageHit: Codable {
    let language: String
    let confidence: Double
}

private struct AborgLanguageResult: Codable {
    let language: String
    let confidence: Double
    let alternatives: [AborgLanguageHit]
}

private func runDetectLanguage(text: String, maxAlternatives: Int) -> AborgLanguageResult? {
    let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
    if trimmed.isEmpty { return nil }
    let recognizer = NLLanguageRecognizer()
    recognizer.processString(trimmed)
    guard let dominant = recognizer.dominantLanguage else { return nil }
    // languageHypotheses returns up to N; we ask for `max+1` so
    // we can drop the dominant entry from the alternatives list
    // and still return up to `max`.
    let raw = recognizer.languageHypotheses(withMaximum: maxAlternatives + 1)
    let dominantConfidence = raw[dominant] ?? 0
    let alternatives: [AborgLanguageHit] = raw
        .filter { $0.key != dominant }
        .sorted { $0.value > $1.value }
        .prefix(maxAlternatives)
        .map { AborgLanguageHit(language: $0.key.rawValue, confidence: $0.value) }
    return AborgLanguageResult(
        language: dominant.rawValue,
        confidence: dominantConfidence,
        alternatives: alternatives
    )
}

@_cdecl("aborg_detect_language")
public func aborg_detect_language(
    _ text: UnsafePointer<CChar>?,
    _ maxAlternatives: Int,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let textStr = text.flatMap { String(validatingCString: $0) } ?? ""
    // Clamp to [0, 16] — more than 16 alternatives is noise from
    // NLLanguageRecognizer; keep the surface small.
    let n = max(0, min(maxAlternatives, 16))
    guard let result = runDetectLanguage(text: textStr, maxAlternatives: n) else {
        // Inconclusive / empty input → success-with-null. The
        // Rust side maps (OK code, null ptr) → `Ok(None)`.
        callback(ctx, nil, 0, kErrCodeOK)
        return
    }
    do {
        let data = try JSONEncoder().encode(result)
        data.withUnsafeBytes { rawBuf in
            let base = rawBuf.baseAddress?.assumingMemoryBound(to: CChar.self)
            callback(ctx, base, data.count, kErrCodeOK)
        }
    } catch {
        let msg = "aborg_detect_language encode error: \(error)\n"
        FileHandle.standardError.write(msg.data(using: .utf8) ?? Data())
        callback(ctx, nil, 0, kErrCodeEncodeFailure)
    }
}

// MARK: - Model install

// Why a separate entry point (not auto-install inside
// `runTranscribe`):
//   - The download is multi-minute the first time; the daemon
//     wants to do this work at Idle priority, not in the middle
//     of an interactive transcribe.
//   - The probe + daemon both need a way to *just* install
//     without transcribing.
// Behaviour: succeeds when status ends up `.installed`. Fails
// when the locale is unsupported or the user's environment
// can't run Apple Intelligence at all.

@available(macOS 26.0, *)
private func runInstallModel(localeStr: String) async throws {
    guard SpeechTranscriber.isAvailable else {
        throw AborgAIError.frameworkUnavailable
    }
    let requested = Locale(identifier: localeStr)
    guard let supported = await SpeechTranscriber.supportedLocale(equivalentTo: requested) else {
        throw AborgAIError.localeUnsupported(localeStr)
    }
    let transcriber = SpeechTranscriber(
        locale: supported,
        transcriptionOptions: [],
        reportingOptions: [],
        attributeOptions: [.audioTimeRange, .transcriptionConfidence]
    )
    let modules: [any SpeechModule] = [transcriber]

    let before = await AssetInventory.status(forModules: modules)
    if before == .installed { return }

    // `assetInstallationRequest` returns nil when no install is
    // needed (already installed, or the system flat-out can't
    // serve this locale). The status check above handles the
    // first case; treat nil from here as "nothing to do, status
    // will say why later".
    if let request = try await AssetInventory.assetInstallationRequest(supporting: modules) {
        try await request.downloadAndInstall()
    }

    let after = await AssetInventory.status(forModules: modules)
    if after != .installed {
        throw AborgAIError.modelNotInstalled(
            "locale=\(supported.identifier) post-install status=\(after)"
        )
    }
}

// MARK: - Locale status query

// Used by `aborg doctor` to surface per-locale install state
// without committing to an install. Returns JSON
// `{"framework_available": bool, "locale_supported": bool,
//   "status": "installed"|"supported"|"downloading"|"unsupported"|"unknown"}`.
//
// `framework_available = false` means Apple Intelligence is
// disabled in System Settings; in that case the other fields
// are still populated with whatever the SDK reports but the
// doctor presents the framework-unavailable diagnosis first.

@available(macOS 26.0, *)
private struct AborgLocaleStatus: Codable {
    let framework_available: Bool
    let locale_supported: Bool
    let status: String
}

@available(macOS 26.0, *)
private func runLocaleStatus(localeStr: String) async -> AborgLocaleStatus {
    let frameworkOK = SpeechTranscriber.isAvailable
    let requested = Locale(identifier: localeStr)
    let supported = await SpeechTranscriber.supportedLocale(equivalentTo: requested)
    let supportedFlag = supported != nil
    let status: String
    if let s = supported {
        let transcriber = SpeechTranscriber(
            locale: s,
            transcriptionOptions: [],
            reportingOptions: [],
            attributeOptions: [.audioTimeRange, .transcriptionConfidence]
        )
        let inv = await AssetInventory.status(forModules: [transcriber])
        status = switch inv {
        case .installed: "installed"
        case .downloading: "downloading"
        case .supported: "supported"
        case .unsupported: "unsupported"
        @unknown default: "unknown"
        }
    } else {
        status = "unsupported"
    }
    return AborgLocaleStatus(
        framework_available: frameworkOK,
        locale_supported: supportedFlag,
        status: status
    )
}

@_cdecl("aborg_speech_locale_status")
public func aborg_speech_locale_status(
    _ locale: UnsafePointer<CChar>?,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let localeStr = locale.flatMap { String(validatingCString: $0) } ?? "en-US"
    guard #available(macOS 26.0, *) else {
        callback(ctx, nil, 0, kErrCodeFrameworkUnavailable)
        return
    }
    Task {
        let report = await runLocaleStatus(localeStr: localeStr)
        do {
            let data = try JSONEncoder().encode(report)
            data.withUnsafeBytes { rawBuf in
                let base = rawBuf.baseAddress?.assumingMemoryBound(to: CChar.self)
                callback(ctx, base, data.count, kErrCodeOK)
            }
        } catch {
            let msg = "aborg_speech_locale_status encode error: \(error)\n"
            FileHandle.standardError.write(msg.data(using: .utf8) ?? Data())
            callback(ctx, nil, 0, kErrCodeEncodeFailure)
        }
    }
}

@_cdecl("aborg_install_speech_model")
public func aborg_install_speech_model(
    _ locale: UnsafePointer<CChar>?,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let localeStr = locale.flatMap { String(validatingCString: $0) } ?? "en-US"

    guard #available(macOS 26.0, *) else {
        callback(ctx, nil, 0, kErrCodeFrameworkUnavailable)
        return
    }

    Task {
        do {
            try await runInstallModel(localeStr: localeStr)
            // Success → callback with the OK code + null payload.
            // No buffer needed; the Rust side only cares about
            // success/failure for install.
            callback(ctx, nil, 0, kErrCodeOK)
        } catch {
            let msg = "aborg_install_speech_model error: \(error)\n"
            FileHandle.standardError.write(msg.data(using: .utf8) ?? Data())
            callback(ctx, nil, 0, errorCode(error))
        }
    }
}

// MARK: - C entry point

@_cdecl("aborg_transcribe_window")
public func aborg_transcribe_window(
    _ inputPath: UnsafePointer<CChar>?,
    _ startSecs: Double,
    _ endSecs: Double,
    _ locale: UnsafePointer<CChar>?,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int, Int32) -> Void
) {
    let pathStr = inputPath.flatMap { String(validatingCString: $0) } ?? ""
    let localeStr = locale.flatMap { String(validatingCString: $0) } ?? "en-US"

    guard #available(macOS 26.0, *) else {
        // Build script targets macOS 26.0, so unreachable when
        // built normally. Defensive call back on the off chance
        // the dylib is loaded on an older host.
        callback(ctx, nil, 0, kErrCodeFrameworkUnavailable)
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
                callback(ctx, base, data.count, kErrCodeOK)
            }
        } catch {
            let msg = "aborg_transcribe_window error: \(error)\n"
            FileHandle.standardError.write(msg.data(using: .utf8) ?? Data())
            callback(ctx, nil, 0, errorCode(error))
        }
    }
}
