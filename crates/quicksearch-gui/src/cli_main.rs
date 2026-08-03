//! `quicksearch-cli <query>` — terminal search, and nothing else.
//!
//! A console-subsystem binary, so redirection, pipes, exit codes, and the
//! shell waiting for the process all behave normally. `src/cli.rs` and
//! `src/format.rs` are shared with the GUI binary by compiling them into both;
//! neither touches egui, so there is nothing to split out into a library.

mod cli;
// The GUI uses more of this module than the CLI does.
#[allow(dead_code)]
mod format;
// The GUI stores/deletes keychain entries; the CLI only reads them.
#[allow(dead_code)]
mod keychain;

fn main() {
    // `maybe_run_cli` returns `None` for "no query given", which the combined
    // binary treats as "open the GUI". This one has no GUI to fall back to.
    let code = cli::maybe_run_cli().unwrap_or_else(|| {
        eprintln!("{}", cli::USAGE);
        2
    });
    std::process::exit(code);
}
