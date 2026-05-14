// Swift FFI bridge for audio sampling.
//
// Currently exposes one entry point: `aborg_audio_read_window`,
// callable from Rust via the static lib produced by
// `crates/audio/build.rs`.
//
// Contract:
//   - Rust passes `(input_path, start_ms, end_ms, sample_rate,
//     ctx, callback)`.
//   - Swift opens the audio file via `AVURLAsset`, decodes the
//     requested time range into mono Float32 PCM at the requested
//     sample rate, and fires the C callback exactly once.
//   - Success → buffer of native-endian Float32 samples, length =
//     samples * 4 bytes; failure → null pointer + length zero +
//     non-zero error code.
//   - `ctx` is opaque (a boxed Rust oneshot sender); passed back
//     unmodified.
//
// Format chosen to match the chromaprint-style fingerprinting in
// `ab-fingerprint`: mono, Float32 PCM, native endian. The caller
// supplies the target sample rate; the underlying
// `AVAssetReaderAudioMixOutput` does the resampling.
//
// AAX is not supported at this slice — `AVURLAsset.load` for an
// AAX file without Audible activation bytes returns an error
// before any track is exposed; the bridge surfaces that as the
// generic asset-load failure (code 2). The transcode-to-m4b
// stage (ADR-0027) handles AAX upstream.

import Foundation
import AVFoundation

// MARK: - Error codes shared with Rust's `BridgeError::from_code`.

private let kErrCodeGeneric: Int32 = 1
private let kErrCodeAssetLoadFailed: Int32 = 2
private let kErrCodeNoAudioTrack: Int32 = 3
private let kErrCodeWindowEmpty: Int32 = 4
private let kErrCodeReadFailure: Int32 = 5
// Transcode-specific (slice C2a). Reserved values for future
// bridge entries continue the integer sequence; never reorder.
private let kErrCodeExportSetupFailed: Int32 = 6
private let kErrCodeExportRunFailed: Int32 = 7

// MARK: - Callback signature
//
// The Swift `@convention(c)` callback type is structurally
// typed; inlining at every use site lets `@_cdecl` see a public
// type without a `public typealias` cluttering the surface.

/// Fire the result callback with the (ctx, bytes, len, code)
/// tuple. The buffer is only borrowed for the duration of the
/// callback; the Rust side copies it before returning.
private func fireCallback(
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int, Int32) -> Void,
    _ data: Data?,
    _ code: Int32
) {
    if let data = data, !data.isEmpty {
        data.withUnsafeBytes { raw in
            let base = raw.baseAddress?.assumingMemoryBound(to: UInt8.self)
            callback(ctx, base, raw.count, code)
        }
    } else {
        callback(ctx, nil, 0, code)
    }
}

// MARK: - Entry point

@_cdecl("aborg_audio_read_window")
public func aborg_audio_read_window(
    inputPath: UnsafePointer<CChar>,
    startMs: UInt64,
    endMs: UInt64,
    sampleRate: UInt32,
    ctx: UnsafeMutableRawPointer?,
    callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int, Int32) -> Void
) {
    // ── Input validation ────────────────────────────────────────────
    let pathString = String(cString: inputPath)
    guard endMs > startMs else {
        fireCallback(ctx, callback, nil, kErrCodeWindowEmpty)
        return
    }
    guard sampleRate > 0 else {
        fireCallback(ctx, callback, nil, kErrCodeWindowEmpty)
        return
    }
    let fileURL = URL(fileURLWithPath: pathString)

    Task.detached {
        let asset = AVURLAsset(url: fileURL)

        // ── Locate the audio track ──────────────────────────────────
        let tracks: [AVAssetTrack]
        do {
            tracks = try await asset.loadTracks(withMediaType: .audio)
        } catch {
            FileHandle.standardError.write(Data(
                "aborg_audio: loadTracks failed for \(pathString): \(error)\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeAssetLoadFailed)
            return
        }
        guard let audioTrack = tracks.first else {
            fireCallback(ctx, callback, nil, kErrCodeNoAudioTrack)
            return
        }

        // ── Configure the reader for mono Float32 at target rate ────
        let outputSettings: [String: Any] = [
            AVFormatIDKey: kAudioFormatLinearPCM,
            AVLinearPCMBitDepthKey: 32,
            AVLinearPCMIsFloatKey: true,
            AVLinearPCMIsNonInterleaved: false,
            AVLinearPCMIsBigEndianKey: false,
            AVSampleRateKey: Double(sampleRate),
            AVNumberOfChannelsKey: 1,
        ]

        let reader: AVAssetReader
        do {
            reader = try AVAssetReader(asset: asset)
        } catch {
            FileHandle.standardError.write(Data(
                "aborg_audio: AVAssetReader init failed for \(pathString): \(error)\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeReadFailure)
            return
        }

        let trackOutput = AVAssetReaderTrackOutput(
            track: audioTrack, outputSettings: outputSettings)
        trackOutput.alwaysCopiesSampleData = false

        guard reader.canAdd(trackOutput) else {
            FileHandle.standardError.write(Data(
                "aborg_audio: canAdd(trackOutput) returned false\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeReadFailure)
            return
        }
        reader.add(trackOutput)

        // ── Restrict the reader's time range to [startMs, endMs) ─────
        let startTime = CMTime(value: CMTimeValue(startMs), timescale: 1_000)
        let endTime = CMTime(value: CMTimeValue(endMs), timescale: 1_000)
        reader.timeRange = CMTimeRange(start: startTime, end: endTime)

        guard reader.startReading() else {
            let err = reader.error
            FileHandle.standardError.write(Data(
                "aborg_audio: startReading failed: \(String(describing: err))\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeReadFailure)
            return
        }

        // ── Drain CMSampleBuffers into one contiguous Data buffer ────
        var accumulator = Data()
        while let sampleBuffer = trackOutput.copyNextSampleBuffer() {
            guard let blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer) else { continue }
            let length = CMBlockBufferGetDataLength(blockBuffer)
            if length == 0 { continue }
            var localBytes = [UInt8](repeating: 0, count: length)
            let copyStatus = localBytes.withUnsafeMutableBytes { raw -> OSStatus in
                guard let base = raw.baseAddress else { return -1 }
                return CMBlockBufferCopyDataBytes(
                    blockBuffer,
                    atOffset: 0,
                    dataLength: length,
                    destination: base)
            }
            if copyStatus != noErr {
                FileHandle.standardError.write(Data(
                    "aborg_audio: CMBlockBufferCopyDataBytes failed (\(copyStatus))\n".utf8))
                fireCallback(ctx, callback, nil, kErrCodeReadFailure)
                return
            }
            accumulator.append(localBytes, count: length)
        }

        switch reader.status {
        case .completed:
            fireCallback(ctx, callback, accumulator, 0)
        case .failed, .cancelled:
            let err = reader.error
            FileHandle.standardError.write(Data(
                "aborg_audio: reader status=\(reader.status.rawValue) err=\(String(describing: err))\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeReadFailure)
        case .reading, .unknown:
            // Shouldn't happen after the drain loop exits, but
            // treat as failure rather than hang the callback.
            FileHandle.standardError.write(Data(
                "aborg_audio: reader status unexpectedly \(reader.status.rawValue) after drain\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeGeneric)
        @unknown default:
            fireCallback(ctx, callback, nil, kErrCodeGeneric)
        }
    }
}

// MARK: - Transcode-to-m4b entry point (slice C2a).
//
// Re-encodes an input audio file to AAC-LC inside an MPEG-4
// container, written to `outputPath`. The .m4b extension is an
// audiobook convention; the on-disk bytes are identical to an
// .m4a (Apple's `appleM4A` preset writes that container). Caller
// chooses .m4b for the file extension.
//
// We use `AVAssetExportSession` with the `appleM4A` preset for
// MVP. The preset's bitrate is fixed (~64kbps mono for typical
// audiobook input); per-bitrate control needs the AVAssetWriter
// path which a future slice can wire in if operators ask for it.
// For ADR-0027's "canonical m4b library" goal the preset's quality
// is already a meaningful drop from typical 128kbps source files.
//
// Failure modes split into two codes so Rust can disambiguate
// "session refused to configure" (codec mismatch, output path
// unwritable) from "session ran but errored mid-export" (decode
// failure, disk full).
@_cdecl("aborg_audio_transcode_to_m4b")
public func aborg_audio_transcode_to_m4b(
    inputPath: UnsafePointer<CChar>,
    outputPath: UnsafePointer<CChar>,
    ctx: UnsafeMutableRawPointer?,
    callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int, Int32) -> Void
) {
    let inputPathString = String(cString: inputPath)
    let outputPathString = String(cString: outputPath)
    let inputURL = URL(fileURLWithPath: inputPathString)
    let outputURL = URL(fileURLWithPath: outputPathString)

    Task.detached {
        let asset = AVURLAsset(url: inputURL)

        // ── Verify the asset has at least one audio track ───────────
        let tracks: [AVAssetTrack]
        do {
            tracks = try await asset.loadTracks(withMediaType: .audio)
        } catch {
            FileHandle.standardError.write(Data(
                "aborg_audio: transcode loadTracks failed for \(inputPathString): \(error)\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeAssetLoadFailed)
            return
        }
        guard !tracks.isEmpty else {
            fireCallback(ctx, callback, nil, kErrCodeNoAudioTrack)
            return
        }

        // ── Remove any stale output (export refuses to overwrite) ───
        // The race window is acceptable: the Rust side picks an
        // output path it owns, and a leftover from a previous
        // failed run is the only realistic source of conflict.
        if FileManager.default.fileExists(atPath: outputPathString) {
            do {
                try FileManager.default.removeItem(at: outputURL)
            } catch {
                FileHandle.standardError.write(Data(
                    "aborg_audio: removeItem(\(outputPathString)) failed: \(error)\n".utf8))
                fireCallback(ctx, callback, nil, kErrCodeExportSetupFailed)
                return
            }
        }

        // ── Configure the export session ────────────────────────────
        guard let session = AVAssetExportSession(
            asset: asset, presetName: AVAssetExportPresetAppleM4A)
        else {
            FileHandle.standardError.write(Data(
                "aborg_audio: AVAssetExportSession init returned nil for preset appleM4A\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeExportSetupFailed)
            return
        }
        session.outputURL = outputURL
        session.outputFileType = .m4a
        // No `audioMix` / `metadata` here — slice C2a is a content
        // re-encode only. Cover art + chapter / tag carryover are
        // C3 (ADR-0028 two-pass tag-write) territory.

        // ── Run the export and await terminal state ─────────────────
        do {
            try await session.export(to: outputURL, as: .m4a)
        } catch {
            FileHandle.standardError.write(Data(
                "aborg_audio: export to \(outputPathString) failed: \(error)\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeExportRunFailed)
            return
        }

        // ── Sanity-check the output exists and is non-empty ─────────
        let attrs = try? FileManager.default.attributesOfItem(atPath: outputPathString)
        let size = (attrs?[.size] as? NSNumber)?.intValue ?? 0
        if size <= 0 {
            FileHandle.standardError.write(Data(
                "aborg_audio: export reported success but output is empty: \(outputPathString)\n".utf8))
            fireCallback(ctx, callback, nil, kErrCodeExportRunFailed)
            return
        }

        // Success — buffer is nil on transcode (the output is a
        // file path, not bytes). Code 0 = success.
        fireCallback(ctx, callback, nil, 0)
    }
}
