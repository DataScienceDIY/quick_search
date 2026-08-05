//! Build identity for the two binaries.
//!
//! Bakes the commit the tree was built from into `QS_COMMIT`, and on Windows
//! compiles a VERSIONINFO resource so the `.exe` reports a version in
//! Explorer's Properties rather than nothing at all.
//!
//! The version number itself is not handled here: `env!("CARGO_PKG_VERSION")`
//! already carries `[workspace.package] version`, which is the one source of
//! truth the CI tag check, build-deb.sh and build-installer.sh all read.
//!
//! No dependencies on purpose — a build script that pulled a crate in would
//! land in Cargo.lock, and every build in this repo runs `--locked`.

use std::path::PathBuf;
use std::process::Command;

/// What the version reads as when there is no git and no `QS_COMMIT` — an
/// unpacked source tarball, say. Never a build failure.
const UNKNOWN: &str = "unknown";

/// Abbreviated-hash length, matching `git rev-parse --short=7` and the hashes
/// the forge shows.
const SHORT_LEN: usize = 7;

fn main() {
    let commit = resolve_commit();
    println!("cargo::rustc-env=QS_COMMIT={commit}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        emit_version_resource(&commit);
    }
}

// ------------------------------------------------------------- commit ----

/// The short commit hash: `QS_COMMIT` first, then git, then [`UNKNOWN`].
///
/// CI sets `QS_COMMIT` from the event's SHA rather than letting this shell out
/// to git, because `actions/checkout` leaves a shallow clone owned by another
/// user — the SHA the runner already knows is both cheaper and more trustworthy
/// than anything read back out of that tree.
fn resolve_commit() -> String {
    println!("cargo::rerun-if-env-changed=QS_COMMIT");
    watch_git_head();

    let supplied = std::env::var("QS_COMMIT").unwrap_or_default();
    let supplied = supplied.trim();
    if !supplied.is_empty() {
        if let Some(hash) = short_hash(supplied) {
            return hash;
        }
        println!(
            "cargo::warning=QS_COMMIT is not a commit hash ({supplied:?}); \
             falling back to git"
        );
    }
    git_commit().unwrap_or_else(|| UNKNOWN.to_string())
}

/// The first [`SHORT_LEN`] characters, lowercased, or `None` when `raw` is not
/// a hex hash. Accepts a full 40-character SHA (what CI passes) and an already
/// abbreviated one alike.
fn short_hash(raw: &str) -> Option<String> {
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // `raw` is ASCII here, so slicing by byte cannot split a character.
    Some(raw[..raw.len().min(SHORT_LEN)].to_ascii_lowercase())
}

fn git_commit() -> Option<String> {
    let out = git(&["rev-parse", &format!("--short={SHORT_LEN}"), "HEAD"])?;
    short_hash(out.trim())
}

/// Rebuild when HEAD moves.
///
/// Without this cargo only reruns the script when a file in the package
/// changes, so committing anything outside this crate would leave the previous
/// hash baked into the binary.
fn watch_git_head() {
    let Some(git_dir) = git_dir() else { return };

    let head = git_dir.join("HEAD");
    watch(&head);
    // On a branch, HEAD itself only changes on checkout — the ref it names is
    // what moves on commit. A detached HEAD holds the hash directly and needs
    // nothing more. The reflog covers the case where the branch ref is packed
    // and so has no loose file to watch.
    if let Ok(contents) = std::fs::read_to_string(&head) {
        if let Some(reference) = contents.trim().strip_prefix("ref:") {
            watch(&git_dir.join(reference.trim()));
        }
    }
    watch(&git_dir.join("logs").join("HEAD"));
}

/// Watch `path`, but only if it exists: cargo treats a `rerun-if-changed` path
/// it cannot stat as permanently dirty, which would recompile this crate on
/// every single build.
fn watch(path: &std::path::Path) {
    if path.exists() {
        println!("cargo::rerun-if-changed={}", path.display());
    }
}

fn git_dir() -> Option<PathBuf> {
    let path = PathBuf::from(git(&["rev-parse", "--absolute-git-dir"])?.trim());
    path.is_dir().then_some(path)
}

/// Run git in the crate directory, `None` on any failure — a missing git, a
/// tree that is not a repository, and a repository git refuses to trust all
/// mean the same thing here.
fn git(args: &[&str]) -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())
        .flatten()
}

// ------------------------------------------------ windows VERSIONINFO ----

/// The binaries this crate builds, with the description Explorer shows in the
/// Properties dialog and in the details pane.
const BINARIES: [(&str, &str); 2] = [
    ("quicksearch", "QuickSearch"),
    ("quicksearch-cli", "QuickSearch terminal search"),
];

/// Compile a VERSIONINFO resource per binary and link it in.
///
/// The string values mirror `packaging/quicksearch.nsi` so the app and the
/// installer that ships it never disagree about who published what.
fn emit_version_resource(commit: &str) {
    // rust-toolchain.toml lists x86_64-pc-windows-gnu and nothing else, and
    // windres is what compiles a .rc there. An MSVC target would need rc.exe
    // and a different invocation, so it is skipped rather than half-supported.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu") {
        return;
    }

    let out_dir = PathBuf::from(env("OUT_DIR"));
    let version = env("CARGO_PKG_VERSION");
    // Windows insists on exactly four numeric fields. Cargo splits the version
    // for us, so unlike build-installer.sh there is no suffix to strip.
    let quad = format!(
        "{},{},{},0",
        env("CARGO_PKG_VERSION_MAJOR"),
        env("CARGO_PKG_VERSION_MINOR"),
        env("CARGO_PKG_VERSION_PATCH"),
    );
    let quad_text = quad.replace(',', ".");

    for (bin, description) in BINARIES {
        let rc = format!(
            r#"1 VERSIONINFO
FILEVERSION {quad}
PRODUCTVERSION {quad}
FILEOS 0x4L
FILETYPE 0x1L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Jeremy <jeremy@karsttech.com>"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{quad_text}"
      VALUE "InternalName", "{bin}"
      VALUE "LegalCopyright", "GPL-3.0-or-later"
      VALUE "OriginalFilename", "{bin}.exe"
      VALUE "ProductName", "QuickSearch"
      VALUE "ProductVersion", "{version} ({commit})"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
        );

        let rc_path = out_dir.join(format!("{bin}.rc"));
        let res_path = out_dir.join(format!("{bin}.res"));
        std::fs::write(&rc_path, rc).expect("OUT_DIR is writable");
        compile_resource(&rc_path, &res_path);

        // Per-binary, because OriginalFilename differs between the two. Linked
        // as a plain object rather than through a static library: nothing
        // references a resource by symbol, so an archive member holding one
        // would be dropped as unused.
        println!("cargo::rustc-link-arg-bin={bin}={}", res_path.display());
    }
}

fn compile_resource(rc: &std::path::Path, res: &std::path::Path) {
    println!("cargo::rerun-if-env-changed=QS_WINDRES");
    let explicit = std::env::var("QS_WINDRES").ok();
    let candidates: Vec<&str> = match &explicit {
        Some(tool) => vec![tool.as_str()],
        // The cross compiler's windres first, then the plain name for a native
        // mingw shell where the tools are unprefixed.
        None => vec!["x86_64-w64-mingw32-windres", "windres"],
    };

    let mut attempts = Vec::new();
    for tool in &candidates {
        match Command::new(tool)
            .arg("-O")
            .arg("coff")
            .arg(rc)
            .arg("-o")
            .arg(res)
            .output()
        {
            Ok(out) if out.status.success() => return,
            Ok(out) => attempts.push(format!(
                "{tool}: exited {} — {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => attempts.push(format!("{tool}: {e}")),
        }
    }
    // A hard error, not a warning. A Windows build already needs mingw for the
    // linker, so this only fires on a genuinely broken toolchain — and silently
    // shipping an .exe with no version is exactly what this exists to prevent.
    panic!(
        "could not compile the Windows version resource. Install \
         binutils-mingw-w64-x86-64 (or set QS_WINDRES to a resource compiler). \
         Tried:\n  {}",
        attempts.join("\n  ")
    );
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("cargo sets {key} for build scripts"))
}
