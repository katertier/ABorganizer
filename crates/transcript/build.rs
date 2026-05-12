//! Compile the Swift FFI bridge into a static library that
//! `ab_transcript` links against.
//!
//! Sets `cfg(aborg_ai_bridge)` on success so the Rust side can
//! gate `extern "C"` declarations + safe wrappers behind a
//! cfg. Non-macOS builds and macOS builds without `swiftc` on
//! PATH still compile the crate; the bridge functions degrade
//! to an `Unavailable` error at runtime.

// xtask: allow_macros — build scripts go via println!.
#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Always declare the cfg so `#[cfg(aborg_ai_bridge)]` doesn't
    // trip the unexpected-cfg lint, even when the bridge isn't
    // built. Compatible with the `--check-cfg` requirement of
    // recent Rust.
    println!("cargo::rustc-check-cfg=cfg(aborg_ai_bridge)");
    println!("cargo:rerun-if-changed=build.rs");

    // Locate the Swift source. ABorganizer keeps Swift in a
    // top-level `swift/` directory (same convention as
    // `~/dev/ABtagger/swift/`).
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let swift_src = manifest_dir
        .join("..")
        .join("..")
        .join("swift")
        .join("aborg_ai.swift");
    println!("cargo:rerun-if-changed={}", swift_src.display());

    // 1. Skip on non-macOS targets entirely. CI on Linux builders
    //    must still link the crate; cfg(not(aborg_ai_bridge))
    //    provides a stubbed `Unavailable`-returning impl.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        println!("cargo:warning=aborg_ai: non-macOS target, bridge disabled");
        return;
    }

    // 2. Skip when swiftc isn't on PATH. Macs without Xcode CLI
    //    tools end up here. The `map_or` form satisfies clippy's
    //    map-unwrap-or lint; semantics: treat spawn failure as
    //    "swiftc not available."
    if Command::new("swiftc")
        .arg("--version")
        .output()
        .map_or(true, |o| !o.status.success())
    {
        println!("cargo:warning=aborg_ai: swiftc not on PATH, bridge disabled");
        return;
    }

    // 3. Source must exist (the swift/ tree is in the repo but a
    //    user could have stripped it).
    if !swift_src.exists() {
        println!(
            "cargo:warning=aborg_ai: {} not found, bridge disabled",
            swift_src.display()
        );
        return;
    }

    // 4. Compile the static library into $OUT_DIR/libaborg_ai.a.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let lib_path = out_dir.join("libaborg_ai.a");

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
            // Speech + AVFoundation are listed even though the
            // 3A.2 stub doesn't use them — that way 3A.3 can
            // swap the stub body without touching the build
            // script. The link is cheap; no symbols pulled in
            // for the stub.
            "-framework",
            "Speech",
            "-framework",
            "AVFoundation",
        ])
        .arg("-o")
        .arg(&lib_path)
        .arg(&swift_src)
        .status();

    let status = match status {
        Ok(s) => s,
        Err(e) => {
            println!("cargo:warning=aborg_ai: swiftc spawn failed: {e}");
            return;
        }
    };
    if !status.success() {
        println!(
            "cargo:warning=aborg_ai: swiftc exited {:?}, bridge disabled",
            status.code()
        );
        return;
    }

    // 5. Emit link directives. cargo will add the libaborg_ai
    //    static archive + the three system frameworks to the
    //    transcript crate's final link line.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=aborg_ai");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Speech");
    println!("cargo:rustc-link-lib=framework=AVFoundation");

    // 6. Flag the bridge as available so the Rust side compiles
    //    its `extern "C"` block.
    println!("cargo::rustc-cfg=aborg_ai_bridge");
}
