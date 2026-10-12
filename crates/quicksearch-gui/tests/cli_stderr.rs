//! Terminal search with its stderr in a pipe nobody reads, as under
//! `2>&1 | head -n1` once `head` has its line: a note that cannot be written
//! is lost, never a panic that turns a finished search into exit 101.

use std::process::{Command, Stdio};

use quicksearch_core::testutil::{seed_index, Scratch, SeedSpec, NEEDLE};

#[test]
fn a_closed_stderr_loses_the_truncation_note_not_the_exit_code() {
    let (dir, db) = Scratch::db("cli-stderr");
    // More files named for the needle than the limit below, so the search is
    // cut short and has its note to write.
    seed_index(
        &db,
        &SeedSpec {
            files: 40,
            dirs: 4,
            needle_names: 4,
            needle_docs: 0,
            body_term_docs: 0,
            ..SeedSpec::default()
        },
    );
    // The per-user config the CLI finds, on either platform.
    let config_home = dir.join("config");
    std::fs::create_dir_all(config_home.join("quicksearch")).unwrap();
    std::fs::write(
        config_home.join("quicksearch").join("config.toml"),
        format!("[paths]\ndatabase_path = '{}'\n", db.display()),
    )
    .unwrap();

    let search = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_quicksearch-cli"));
        cmd.args(["--limit", "1", NEEDLE])
            .env("XDG_CONFIG_HOME", &config_home)
            .env("APPDATA", &config_home)
            .stdin(Stdio::null());
        cmd
    };

    // Read, stderr has the note: the path under test is taken.
    let out = search().output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains("truncated at 1 results"),
        "stderr: {stderr}"
    );

    // The read end is gone before the child starts, so every stderr write
    // fails as a broken pipe; stdout is read as usual.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = search().stderr(writer).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}");
    assert_eq!(stdout.lines().count(), 1, "one hit, as limited: {stdout}");
}
