//! Compile the Swift FFI bridge for the Apple Speech +
//! Natural Language frameworks into a static library that
//! `ab_speech` links against.
//!
//! Sets `cfg(aborg_speech_bridge)` on success so the Rust side
//! can gate `extern "C"` declarations + safe wrappers behind a
//! cfg. Non-macOS builds and macOS builds without `swiftc` on
//! PATH still compile the crate; the bridge functions degrade
//! to a `BridgeError::BridgeUnavailable` at runtime.

// xtask: allow_macros — build scripts go via println!.
#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Always declare the cfg so `#[cfg(aborg_speech_bridge)]`
    // doesn't trip the unexpected-cfg lint even when the bridge
    // isn't built. Required by recent Rust's `--check-cfg`.
    println!("cargo::rustc-check-cfg=cfg(aborg_speech_bridge)");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let swift_src = manifest_dir
        .join("..")
        .join("..")
        .join("swift")
        .join("aborg_speech.swift");
    println!("cargo:rerun-if-changed={}", swift_src.display());

    // 1. Non-macOS: stub. The Rust side returns BridgeUnavailable.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        println!("cargo:warning=aborg_speech: non-macOS target, bridge disabled");
        return;
    }

    // 2. swiftc must be on PATH. Same map_or idiom as transcript.
    if Command::new("swiftc")
        .arg("--version")
        .output()
        .map_or(true, |o| !o.status.success())
    {
        println!("cargo:warning=aborg_speech: swiftc not on PATH, bridge disabled");
        return;
    }

    // 3. Source must exist.
    if !swift_src.exists() {
        println!(
            "cargo:warning=aborg_speech: {} not found, bridge disabled",
            swift_src.display()
        );
        return;
    }

    // 4. Compile into $OUT_DIR/libaborg_speech.a.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let lib_path = out_dir.join("libaborg_speech.a");

    // Target triple: macOS 26.0+ Apple Silicon (project's hard
    // minimum, see PROJECT.md). swiftc 6.3 reports its native
    // target as `arm64-apple-macosx26.0` — use the same shape
    // here. Older minor versions are still callable thanks to
    // the platform's ABI stability.
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
            "Speech",
            "-framework",
            "AVFoundation",
            // NaturalLanguage for `NLLanguageRecognizer`
            // (`aborg_detect_language` entry point — pre + post-
            // transcribe language detection).
            "-framework",
            "NaturalLanguage",
        ])
        .arg("-o")
        .arg(&lib_path)
        .arg(&swift_src)
        .status();

    let status = match status {
        Ok(s) => s,
        Err(e) => {
            println!("cargo:warning=aborg_speech: swiftc spawn failed: {e}");
            return;
        }
    };
    if !status.success() {
        println!(
            "cargo:warning=aborg_speech: swiftc exited {:?}, bridge disabled",
            status.code()
        );
        return;
    }

    // 5. Emit link directives. cargo will add the libaborg_speech
    //    static archive + the four system frameworks to the
    //    speech crate's final link line.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=aborg_speech");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Speech");
    println!("cargo:rustc-link-lib=framework=AVFoundation");
    println!("cargo:rustc-link-lib=framework=NaturalLanguage");

    // 6. The Swift runtime rpath that downstream binaries need
    //    lives in `.cargo/config.toml` (the per-target
    //    `rustflags` block). `cargo:rustc-link-arg=` from a
    //    build script only applies to its own crate's
    //    targets — not to downstream binaries that link the
    //    static lib. Tested: without the config-level rpath,
    //    `aborg-tools::transcribe-probe` fails to launch with
    //    "Library not loaded: @rpath/libswift_Concurrency.dylib".

    // 7. Flag the bridge as available so the Rust side compiles
    //    its `extern "C"` block.
    println!("cargo::rustc-cfg=aborg_speech_bridge");
}
