//! Build script.
//!
//! Compiles the Swift FFI bridge (`swift/aborg_audio.swift`) into
//! a static library that `ab_audio` links against. The bridge
//! exposes `aborg_audio_read_window` for windowed Float32 PCM
//! decode via `AVAssetReader` — the audio side of the audiologo
//! detection path (slice 4B / ADR-0024 Revision 2).
//!
//! Sets `cfg(ab_audio_bridge)` on success so the Rust side can
//! gate `extern "C"` declarations + safe wrappers behind a cfg.
//! Non-macOS builds and macOS builds without `swiftc` on PATH
//! still compile the crate; the bridge functions degrade to
//! `BridgeError::BridgeUnavailable` at runtime.
//!
//! Mirrors `crates/speech/build.rs` and `crates/foundation-models
//! /build.rs` so all three Swift bridges follow one pattern.

// xtask: allow_macros — Cargo build-script directives go via println!.
#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Always declare the cfg so `#[cfg(ab_audio_bridge)]` doesn't
    // trip the unexpected-cfg lint even when the bridge isn't
    // built. Required by recent Rust's `--check-cfg`.
    println!("cargo::rustc-check-cfg=cfg(ab_audio_bridge)");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let swift_src = manifest_dir
        .join("..")
        .join("..")
        .join("swift")
        .join("aborg_audio.swift");
    println!("cargo:rerun-if-changed={}", swift_src.display());

    // 1. Non-macOS: stub. Rust returns BridgeUnavailable.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        println!("cargo:warning=ab_audio: non-macOS target, bridge disabled");
        return;
    }

    // 2. swiftc must be on PATH.
    if Command::new("swiftc")
        .arg("--version")
        .output()
        .map_or(true, |o| !o.status.success())
    {
        println!("cargo:warning=ab_audio: swiftc not on PATH, bridge disabled");
        return;
    }

    // 3. Source must exist.
    if !swift_src.exists() {
        println!(
            "cargo:warning=ab_audio: {} not found, bridge disabled",
            swift_src.display()
        );
        return;
    }

    // 4. Compile into $OUT_DIR/libaborg_audio.a.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let lib_path = out_dir.join("libaborg_audio.a");

    // Target triple: macOS 26.0+ Apple Silicon (project's hard
    // minimum, see PROJECT.md). Matches the speech + fm bridges.
    let target = "arm64-apple-macosx26.0";

    let status = Command::new("swiftc")
        .args([
            "-emit-library",
            "-static",
            "-parse-as-library",
            "-O",
            "-target",
            target,
            "-framework",
            "Foundation",
            "-framework",
            "AVFoundation",
            "-framework",
            "CoreMedia",
        ])
        .arg("-o")
        .arg(&lib_path)
        .arg(&swift_src)
        .status();

    let status = match status {
        Ok(s) => s,
        Err(e) => {
            println!("cargo:warning=ab_audio: swiftc spawn failed: {e}");
            return;
        }
    };
    if !status.success() {
        println!(
            "cargo:warning=ab_audio: swiftc exited {:?}, bridge disabled",
            status.code()
        );
        return;
    }

    // 5. Emit link directives.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=aborg_audio");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=AVFoundation");
    println!("cargo:rustc-link-lib=framework=CoreMedia");

    // 6. Swift runtime rpath lives in `.cargo/config.toml` (the
    //    per-target `rustflags` block). `cargo:rustc-link-arg=`
    //    from a build script only applies to the build script's
    //    own crate's targets, not downstream binaries that link
    //    the static lib. See `crates/speech/build.rs` step 6 for
    //    the historical rationale.

    // 7. Flag the bridge as available.
    println!("cargo::rustc-cfg=ab_audio_bridge");
}
