//! Workspace check runner.
//!
//! Invoked from CI as `cargo xtask check`. Runs every check; a single
//! failure exits with status 1 so PR jobs surface the issue.

// xtask: allow_macros — this binary's job is to print to stderr.

// xtask: allow_macros — this tool's job is to print to stderr.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    // Internal modules use pub(crate) for cross-module access; clippy
    // flags this as redundant inside a private module, but it's the
    // canonical visibility for "use within this crate only".
    clippy::redundant_pub_crate
)]

mod checks;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "ABorganizer workspace checks")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run every check.
    Check,
    /// Run only the banned-identifiers check.
    Names,
    /// Run only the banned-macros check.
    Macros,
    /// Run only the hardcoded-app-name check.
    Brand,
    /// Run only the route-test-coverage check.
    RouteTests,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = match args.cmd {
        Cmd::Check => checks::run_all(),
        Cmd::Names => checks::names::run(),
        Cmd::Macros => checks::macros::run(),
        Cmd::Brand => checks::brand::run(),
        Cmd::RouteTests => checks::route_tests::run(),
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(n) => {
            eprintln!("xtask check: {n} issue(s) found");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("xtask error: {e:#}");
            ExitCode::from(2)
        }
    }
}
