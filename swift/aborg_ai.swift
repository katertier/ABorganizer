// Swift FFI bridge for the ABorganizer AI features.
//
// Currently exposes one entry point: `aborg_transcribe_window`,
// callable from Rust via the static lib produced by
// `crates/transcript/build.rs`. The slice 3A.2 implementation
// is a stub — it returns a single sentinel segment so the
// link path can be exercised end-to-end before SpeechAnalyzer
// integration lands in slice 3A.3.
//
// Contract:
//   - Rust passes `(input_path, start_secs, end_secs, locale, ctx,
//     callback)`.
//   - Swift computes the result asynchronously and fires the C
//     callback exactly once with a JSON-encoded `[Segment]`.
//   - `ctx` is opaque to Swift — a boxed Rust oneshot sender,
//     passed back unmodified.
//
// JSON shape:
//   [{"start_ms": u64, "end_ms": u64, "text": String, "confidence": f32}, ...]
//
// On error the callback receives `nil` for the data pointer.

import Foundation

private let stubResultJSON = #"""
[{"start_ms":0,"end_ms":1000,"text":"[transcribe stub]","confidence":0.0}]
"""#

@_cdecl("aborg_transcribe_window")
public func aborg_transcribe_window(
    _ inputPath: UnsafePointer<CChar>?,
    _ startSecs: Double,
    _ endSecs: Double,
    _ locale: UnsafePointer<CChar>?,
    _ ctx: UnsafeMutableRawPointer?,
    _ callback: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?, Int) -> Void
) {
    // 3A.2 stub: synchronous callback with the sentinel JSON.
    // 3A.3 replaces this body with a SpeechAnalyzer task that
    // calls back from the completion handler.
    //
    // The arguments are accepted (and ignored here) so the C ABI
    // and Rust safe wrapper don't change between slices.
    _ = inputPath
    _ = startSecs
    _ = endSecs
    _ = locale

    stubResultJSON.withCString { ptr in
        // `utf8.count` is the byte length — the Rust side uses
        // this to bound the read; the buffer is NOT
        // nul-terminated-required.
        callback(ctx, ptr, stubResultJSON.utf8.count)
    }
}
