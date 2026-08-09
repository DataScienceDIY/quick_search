//! `quicksearch-cli <query>` — terminal search, and nothing else.
//!
//! A console-subsystem binary, so redirection, pipes, exit codes, and the
//! shell waiting for the process all behave normally. `src/cli.rs` and
//! `src/format.rs` are compiled into both binaries.

mod cli;
#[allow(dead_code)]
mod format;
#[allow(dead_code)]
mod keychain;
#[allow(dead_code)]
mod version;

fn main() {
    // `None` means "no query given"; this binary has no GUI to fall back to.
    let code = cli::maybe_run_cli().unwrap_or_else(|| {
        eprintln!("{}", cli::USAGE);
        2
    });
    std::process::exit(code);
}
