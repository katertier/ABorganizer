//! Build script.
//!
//! Today: declares the custom cfg flag `ab_audio_bridge` so the
//! Rust 2024 `unexpected_cfgs` lint accepts conditional compilation
//! gated on a successful Swift bridge build.
//!
//! Future: compile the Swift bridge static library, link it in, and
//! emit `cargo::rustc-cfg=ab_audio_bridge` so callers can detect
//! the bridge is available.

// xtask: allow_macros — Cargo build-script directives go via println!.

fn main() {
    // Declare our custom cfg flag(s). Future builds will conditionally
    // emit `cargo::rustc-cfg=ab_audio_bridge` when the Swift bridge
    // compiles.
    println!("cargo::rustc-check-cfg=cfg(ab_audio_bridge)");
}
