//! Internal helpers shared between aborg-tools binaries.
//!
//! Most per-aspect benches keep everything in a single file
//! (see `src/bin/`). When a binary outgrows that — currently
//! only `audiologo-audit` does — its supporting modules land
//! under `src/<binary_name>/` and bin entry imports via this
//! lib crate.

#![allow(missing_docs)] // bench-style binaries; docs as needed

pub mod audit;
