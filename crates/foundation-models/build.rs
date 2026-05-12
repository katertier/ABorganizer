//! Compile the Swift FFI bridge for the Apple Intelligence
//! Foundation Models framework into a static library that
//! `ab_foundation_models` links against.
//!
//! Sets `cfg(aborg_fm_bridge)` on success so the Rust side can
//! gate `extern "C"` declarations + safe wrappers behind a cfg.
//! Non-macOS builds and macOS builds without `swiftc` on PATH
//! (or with the framework missing on the SDK) still compile the
//! crate; the bridge functions degrade to a
//! `BridgeError::BridgeUnavailable` at runtime.
//!
//! Mirrors `crates/transcript/build.rs` — same staging, same
//! warning conventions. Keep them in sync.

// xtask: allow_macros — build scripts go via println!.
#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Always declare the cfg so `#[cfg(aborg_fm_bridge)]` doesn't
    // trip the unexpected-cfg lint even when the bridge isn't
    // built. Required by recent Rust's `--check-cfg`.
    println!("cargo::rustc-check-cfg=cfg(aborg_fm_bridge)");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let swift_src = manifest_dir
        .join("..")
        .join("..")
        .join("swift")
        .join("aborg_fm.swift");
    println!("cargo:rerun-if-changed={}", swift_src.display());

    // 1. Non-macOS: stub. The Rust side returns BridgeUnavailable.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        println!("cargo:warning=aborg_fm: non-macOS target, bridge disabled");
        return;
    }

    // 2. swiftc must be on PATH. Same map_or idiom as transcript.
    if Command::new("swiftc")
        .arg("--version")
        .output()
        .map_or(true, |o| !o.status.success())
    {
        println!("cargo:warning=aborg_fm: swiftc not on PATH, bridge disabled");
        return;
    }

    // 3. Source must exist.
    if !swift_src.exists() {
        println!(
            "cargo:warning=aborg_fm: {} not found, bridge disabled",
            swift_src.display()
        );
        return;
    }

    // 4. Compile into $OUT_DIR/libaborg_fm.a.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let lib_path = out_dir.join("libaborg_fm.a");

    // Target triple: macOS 26.0+ Apple Silicon. The Foundation
    // Models framework is only available from macOS 26 (Tahoe);
    // older OS hosts will see the runtime check fail and the
    // bridge return `BridgeUnavailable`.
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
            // The actual model surface. Apple ships this as
            // `FoundationModels.framework` on macOS 26+. The
            // Swift source `@available` guards run-time access
            // so a build on a SDK that lacks the framework will
            // produce a binary that just returns BridgeUnavailable.
            "-framework",
            "FoundationModels",
        ])
        .arg("-o")
        .arg(&lib_path)
        .arg(&swift_src)
        .status();

    let status = match status {
        Ok(s) => s,
        Err(e) => {
            println!("cargo:warning=aborg_fm: swiftc spawn failed: {e}");
            return;
        }
    };
    if !status.success() {
        println!(
            "cargo:warning=aborg_fm: swiftc exited {:?}, bridge disabled",
            status.code()
        );
        return;
    }

    // 5. Link directives — the static archive + system framework.
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=aborg_fm");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=FoundationModels");

    // 6. Swift runtime rpath for downstream binaries comes from
    //    workspace `.cargo/config.toml` rustflags — see
    //    `crates/transcript/build.rs` step 6 for rationale.

    // 7. Flag the bridge as available.
    println!("cargo::rustc-cfg=aborg_fm_bridge");
}
