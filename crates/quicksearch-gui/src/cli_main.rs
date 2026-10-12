//! `quicksearch-cli <query>`: terminal search, and nothing else.
//!
//! A console-subsystem binary, so redirection, pipes, exit codes, and the
//! shell waiting for the process all behave normally. `src/cli.rs` and
//! `src/format.rs` are compiled into both binaries.

// Per-binary, like the GUI's; see `platform::Allocator`.
#[global_allocator]
static GLOBAL: quicksearch_core::platform::Allocator = quicksearch_core::platform::Allocator;

mod cli;
#[allow(dead_code)]
mod format;
#[allow(dead_code)]
mod keychain;
#[allow(dead_code)]
mod version;

use std::io::Write;

fn main() {
    // `None` means "no arguments"; this binary has no GUI to fall back to.
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let code = cli::maybe_run_cli(&args).unwrap_or_else(|| {
        // Not `eprintln!`, which panics on a closed stderr (`src/cli.rs`).
        let _ = writeln!(std::io::stderr(), "{}", cli::USAGE);
        2
    });
    std::process::exit(code);
}
