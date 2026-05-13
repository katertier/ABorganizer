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
