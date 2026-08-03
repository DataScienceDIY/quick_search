# QuickSearch

A fast local file indexer and search tool. QuickSearch walks your chosen
folders into a compact SQLite index (FTS5 full-text + zstd-compressed text
sidecar), keeps it fresh automatically with filesystem watchers and
periodic reindexing, and serves ranked search-as-you-type results in a
compact egui desktop app, or straight to your terminal.

## Build & run

```sh
./build.sh          # Linux
build.bat           # Windows
```

These take a fresh machine all the way to a running app: install whatever
build dependencies are missing, build release, then launch the GUI. Each
setup stage is skipped when what it provides is already there, so a normal
run costs one `cargo build`. `build.sh` installs Linux system packages with
the distribution's package manager via `sudo` (apt/dnf/pacman/zypper) and the
Rust toolchain with rustup; `build.bat` uses winget and rustup. Both take
`--check` to report dependency status without installing or building,
`--no-run` to stop after the build, and `--` to pass the rest to the binary.

Building by hand needs a Rust toolchain (edition 2021) plus, on every
platform, a C toolchain and Perl: SQLCipher, zstd and OpenSSL are compiled
from bundled C sources, and OpenSSL's `Configure` is a Perl script. The old
WebKit/WebView dependencies (`setup.sh`) are gone; the GUI renders with
OpenGL via egui.

- Linux: working OpenGL 3.3 drivers; `xdg-desktop-portal` (present on all
  mainstream desktops) provides the native folder picker. On minimal
  images you may need `build-essential perl pkg-config`. No X11, Wayland or
  xkbcommon `-dev` packages are needed: winit dlopens the display stack at
  run time, so only the runtime libraries matter.
- Windows: Visual Studio 2022 Build Tools with the "Desktop development
  with C++" workload (MSVC v143 plus a Windows SDK), and Perl (Strawberry
  Perl); NASM is optional and only enables OpenSSL's assembly paths. For the
  GNU target instead, `rustup target add x86_64-pc-windows-gnu` and a
  mingw-w64 toolchain. Note that Windows ships only a software OpenGL 1.1
  driver, so a bare VM or an RDP session without a vendor GPU driver cannot
  create a context and the window will fail to open.
- macOS: Xcode command line tools (`build.sh` does not auto-install these —
  only Linux package managers are handled).

```sh
cargo build --release -p quicksearch-gui   # binaries: target/release/quicksearch{,-cli}
cargo run -p quicksearch-gui               # or just run it
cargo test -p quicksearch-core             # backend test suite
```

Two binaries are produced. `quicksearch` is the desktop app; on Windows it
is built as a window-subsystem app so no console appears behind it.
`quicksearch-cli` is terminal search — a console app, so pipes, redirection
and exit codes behave normally. On Unix `quicksearch` also does both, and
`quicksearch-cli` is simply the same tool under a clearer name.

## Install (Debian / Ubuntu)

```sh
./packaging/build-deb.sh
sudo apt install ./dist/quicksearch_0.1.0-1_amd64.deb
```

The script builds the release binary, strips it, and assembles a `.deb` with
`dpkg-deb`. It needs no `cargo-deb`, no `debhelper` and no SVG rasteriser —
only `dpkg-deb` and `desktop-file-utils`, both standard on Debian and Ubuntu.
Useful flags: `--no-build` to package a binary you already built, `--no-strip`
to keep debug symbols, `-o DIR` to write elsewhere. `DEB_REVISION` and
`DEB_MAINTAINER` override the packaging revision and maintainer.

The package installs:

| Path | Contents |
| --- | --- |
| `/usr/bin/quicksearch` | the desktop app, which also does terminal search |
| `/usr/bin/quicksearch-cli` | terminal search only |
| `/usr/share/applications/quicksearch.desktop` | menu entry, so QuickSearch appears in the app launcher |
| `/usr/share/icons/hicolor/{16,22,24,32,48,64,128,256}x*/apps/` | icons at each size |
| `/usr/share/icons/hicolor/scalable/apps/quicksearch.svg` | the source icon |
| `/usr/share/man/man1/quicksearch{,-cli}.1.gz` | `man quicksearch`; the `-cli` page is a `.so` stub pointing at it |
| `/usr/share/doc/quicksearch/` | copyright, changelog, README, `config_example.toml` |

Installing registers the menu entry and the icon: dpkg triggers owned by
`desktop-file-utils` and `hicolor-icon-theme` refresh both caches, so no
maintainer scripts are involved and `apt remove` reverses it cleanly.

No `config.toml` is installed. One placed next to the executable would put
every user into portable mode (see [Configuration](#configuration)); instead
the app writes `~/.config/quicksearch/config.toml` on first run.

### Icons

`crates/quicksearch-gui/assets/icons/` holds `quicksearch_icon.svg` and the
PNGs rasterised from it. The PNGs are committed rather than generated, so an
ordinary `cargo build` needs no image tooling — the 256px one is compiled into
the binary with `include_bytes!` and becomes the window icon. Editing the SVG
means re-rendering the PNGs; `packaging/build-deb.sh` documents how in a
comment at the top.

X11 takes the window icon from the embedded PNG. Wayland ignores it and
matches the app id (`quicksearch`) against the installed
`quicksearch.desktop`, so under Wayland the titlebar icon appears only once
the package is installed.

## Usage

### GUI

`quicksearch` with no query arguments opens the app:

- **Search**: results appear as you type; every keystroke cancels the
  previous search. One checkbox enables the two fuzzy passes. Sort by
  rank, name, path, size, or modified. Double-click a result to open it;
  right-click it to reveal it in the file manager, open it, copy its
  path, or build an ignore filter from it (session-only by default,
  optionally persisted to the config). Result text can be selected and
  copied in place. Matches in file contents show highlighted snippets.
- **Manage Index**: full indexing status, Start/Stop/Automatic controls,
  indexed folder list, full-text extension filters, ignore patterns, and
  the indexing options. Stopping switches to manual mode and saves that
  (`indexing.auto_index`), so a stopped index stays stopped across
  restarts until you return to automatic. The index size beside the
  status heading totals the database and its `-wal`/`-shm` sidecars,
  refreshed every ten seconds; hovering it lists the ways to make it
  smaller.
- **Duplicates**: files sharing a content hash, grouped.
- **Logs**: the lines the app would have printed to a terminal — warnings
  from indexing, folder watching and opening files, newest last, with a
  filter box and Copy button. Launched from a desktop launcher (or on
  Windows, where the app has no console at all) this is the only place
  they are visible.
- **Help**: an in-app quickstart — first indexing run, example queries,
  what each tab does — pointing here for everything technical.

The bottom status bar always shows what the indexer is doing (phase,
percent, files/sec) or the total indexed file count when idle.

### Terminal

```sh
quicksearch report type:Document modified:">=2024-01-01"
quicksearch --long --limit 20 "quarterly budget"
quicksearch --fuzzy repot          # tolerates typos
```

Prints rank-ordered paths (pipe-friendly); `--long` adds rank, size,
mtime, and highlighted snippets. `quicksearch --help` shows all flags.

On Windows use `quicksearch-cli` for all of the above — `quicksearch.exe`
opens the app, and any query given to it seeds the search box instead of
printing. Colour in `--long` output needs a console with virtual-terminal
processing; Windows Terminal has it, and older consoles get plain text.

### Password protection

The index contains the names and (by default) the full text of everything
it indexes — for most setups, your entire home directory. That is a lot of
concentrated risk in one file. **Options → Security → Enable password
protection** encrypts the index on disk with SQLCipher; from then on
QuickSearch asks for the password every time it starts, in the GUI (an
unlock screen before anything opens the index) and in the terminal (a
hidden prompt). Enabling, disabling, or changing the password deletes and
rebuilds the index — there is no in-place conversion.

- The key is derived as `Argon2id(password, salt)`; the salt is written to
  `config.toml` when the password is set (it is unique, not secret, and
  required — keep it with the config if you copy a protected setup).
- **Remember on this device** stores the derived key (never the password)
  in the OS keychain — Secret Service/KWallet on Linux, Credential Manager
  on Windows — and skips the prompt. Without a keychain daemon the option
  quietly falls back to prompting.
- Scripts can set `QUICKSEARCH_PASSWORD` for non-interactive terminal
  search. Environment variables are readable by other processes of the
  same user (`/proc/<pid>/environ`) — prefer the keychain.
- Forgot the password? The unlock screen can delete the index and disable
  protection; your files are untouched and re-indexing rebuilds it.

What this protects: the index file at rest — disk theft, backups, other
accounts reading the file. What it does not protect: a compromised running
session (the derived key is in process memory while the app runs), and the
files themselves, which are exactly as readable as before. A wrong
password can never wipe the index; it is refused without touching the
file.

### Query syntax

This section is the complete reference (the in-app "?" popup shows a
condensed version of the same rules). Everything that isn't a filter is
matched as one phrase, in order. Filters combine freely with the search
text:

| Syntax | Meaning |
|---|---|
| `budget report` | names, contents, and paths containing the phrase `budget report` |
| `"exact phrase"` | quotes keep spaces, stars, and filter-like words literal; `""` escapes a quote |
| `bud*port` | `*` matches any run of characters (it stays on one line of content); `%` and `_` are always literal |
| `regex:"(foo|bar)\d+"` | regular expression matched against names, contents, and paths; case-insensitive by default (`(?-i:…)` overrides); quote patterns containing spaces or `( ) : = < > "` |
| `type:Audio` | one of Audio, Image, Video, Document, Text, Archive, Spreadsheet, Presentation, Folder |
| `modified:>=2024-01-01` | also `<`, `<=`, `>`, `=` (dates are `yyyy-mm-dd`; `mtime:` is an alias) |
| `path:/home/me/docs` | restrict to a folder and its subfolders (`folder:` and `includefolder:` are aliases); `*` is literal here |
| `path:C:\Users\me\docs` | the same on Windows — drive letters and backslashes need no quoting |
| `mime:application/pdf` | exact MIME type |
| `name:re*.txt` | filename contains (as a filter, unranked; `filename:` is an alias); unquoted `*` globs |

Unrecognized `key:value` text (like `12:30`) stays part of the search
phrase, and a half-typed quote never errors while you type. `AND`, `OR`
and parentheses are treated as plain words. A term of only stars matches
nothing, and a regex that could match the empty string is rejected rather
than matching every file. `regex:` bypasses the trigram index entirely
and combines with filters; alongside search text it acts as an extra
requirement on those results.

The search box highlights this syntax as you type: recognized filter
keywords in red, their arguments in blue, syntax characters (operators,
quotes, live wildcards) in green, on a tinted chip per complete filter.
An argument the engine would reject — unknown `type:` name, bad date,
invalid regex — switches to the error color immediately.

Results are ranked: exact filename matches (case-sensitive first), then
filename substrings, then full-text matches ordered by occurrence count,
then fuzzy filename/full-text matches when enabled, and last the files
matched somewhere else in their path. Later, weaker matches only ever
append to the bottom of the list. Wildcard terms rank through the same
tiers (an "exact" match means the whole name matches the pattern) but
skip the fuzzy passes; regex-only queries reuse the substring, full-text,
and path tiers. Path matching needs at least three characters, and terms
may span separators (`docs/report`). Full-text matching also needs at
least three characters of literal text (the trigram floor). The fuzzy
passes tolerate typos with a budget of one edit per three characters,
capped by `[search] fuzzy_max_edits` (default 2; 0 turns fuzzy off).

### Configuration

`config.toml` lives at `~/.config/quicksearch/config.toml` (Windows:
`%APPDATA%\quicksearch\config.toml`) and is created on first run; the
default index goes to `~/.local/share/quicksearch/index.sqlite`
(Windows: `%LOCALAPPDATA%\quicksearch\index.sqlite`). See
`config_example.toml` for every option.

Defaults follow the platform. The first indexing root is your home
directory or `%USERPROFILE%`; `include_hidden = false` skips dot-files
everywhere and additionally anything marked Hidden or System on Windows,
which is what keeps `AppData`, `$RECYCLE.BIN` and `System Volume
Information` out of the index; and ignore patterns are matched
case-insensitively on Windows and macOS, matching the filesystem.

**Portable mode**: a `config.toml` sitting next to the `quicksearch`
binary overrides the user config entirely, and relative paths inside any
config resolve against the config file's own directory, so a folder
containing the binary, its config, and its index can be moved wholesale.

The GUI edits the config live; external edits apply on next start.

## Engineering overview

Two crates:

```
crates/quicksearch-core   library: indexing, storage, search
crates/quicksearch-gui    binary "quicksearch": egui app + terminal mode
```

### Backend (`quicksearch-core`)

Synchronous Rust: `std::thread` + `mpsc` channels, no async runtime.

- **Storage** (`db/`): SQLite via rusqlite (bundled SQLCipher build —
  identical to stock SQLite until a key is applied), WAL mode so the
  single writer never blocks streaming read-only searches. A run forces a
  `wal_checkpoint(TRUNCATE)` every `processing.maximum_wal_size` bytes of
  log, because SQLite's own autocheckpoint can only reset the log at an
  instant no reader holds it — and a run keeps a reader per root querying
  throughout, so left alone the log grows for the whole run. `files` holds
  metadata (name, path, size, mtime, hash, MIME/type bitmask, per-row
  index state); `searchabletext` is a *contentless* FTS5 table (postings
  only, configurable tokenizer, trigram by default); canonical extracted
  text lives zstd-compressed in `documents_text`, which powers snippets,
  occurrence ranking, and fuzzy full-text search. Schema changes wipe and
  rebuild by policy; the indexer (`open_or_recreate`) is the only code
  allowed to do that; every consumer uses `open_existing`, which treats
  drift as an error, never data loss. With password protection on, every
  open applies the Argon2id-derived raw key (`security.rs`, process-global
  in `db/key.rs`) before anything reads the file; a wrong key is a tagged
  `KEY_MISMATCH` error, structurally distinct from the schema drift that
  may wipe, so it can never destroy an intact index.
- **Indexing** (`indexing.rs`, `file_handling.rs`): full runs walk each
  root (`filtered_walk` prunes hidden/ignored subtrees before descending),
  classify files by mtime into insert/update/skip, batch-write metadata,
  sweep stale rows, then extract content (plaintext, RTF, Office, PDF,
  audio tags, EXIF; see `extract/`) for FTS. Files whose extension no MIME
  table knows — including extensionless ones like `README` or `Makefile` —
  are sniffed from their head bytes and indexed as text when they read as
  text (`mime.rs`, `textenc.rs`); non-UTF-8 text (UTF-16 with BOM, legacy
  charsets via chardetng) is decoded and stored as UTF-8. More claimed
  files means a bigger index — `indexing.content_extensions` remains the
  throttle. Files no larger than `processing.hash_length` skip that second
  pass entirely: the head the walk reads to hash them is already their
  whole content, so a plaintext body is extracted in the same `read` and
  stored complete. Every run ends — whether
  it completed or was stopped — with an optimize pass on its own connection:
  checkpoint, VACUUM if the file has at least 10% slack to reclaim, `PRAGMA
  optimize`, checkpoint again. Progress streams through a polled
  `IndexingStatus`, which reads `Optimizing` for the duration of that pass.
- **Coordinator** (`coordinator.rs`): the object binaries construct.
  Owns the `IndexingService`, the debouncing filesystem watcher
  (`watcher.rs`), and the mode state machine (Auto / Manual, persisted as
  `indexing.auto_index` — the mode the app is left in is the mode it
  starts in, and a config carrying a different value switches it). Watcher
  events become single-file transactions (`incremental.rs`) that keep
  `files`, FTS, and the text sidecar consistent per commit; a full
  reindex runs on a configurable interval. Incremental writes defer while
  a full run is active, so there is exactly one writer at a time.
  Registration follows what the platform's notification API can do:
  inotify covers one directory per watch, so the roots are walked and each
  surviving directory registered individually (skipping `.git`,
  `node_modules` and hidden subtrees, which is what keeps the watch count
  affordable), while `ReadDirectoryChangesW` covers a whole tree from one
  handle and takes a single watch per root, filtering the events instead.
  Either way a tree too large to watch degrades to periodic reindexing
  rather than going silently stale.
- **Search** (`search/`): `SearchService` runs one worker thread; each
  query is a *generation*. New queries interrupt the in-flight SQLite
  statement (`InterruptHandle`) and stale generations stop cooperatively,
  so typing never waits. The cascade streams rank-ordered batches: one
  `files` scan classifies exact/case/substring filename matches (ranks
  1–4) and, since a path contains its own name, sets aside full-path
  matches from the same rows (ranks 9–10); one FTS phrase probe verified
  against the decompressed text yields full-text ranks 5–6 ordered by
  occurrence count; and the opt-in fuzzy passes run a bitap (Wu–Manber)
  matcher over filenames (rank 7), document text (rank 8) and paths
  (rank 11), with a configurable edit budget. The deferred path tiers
  flush last, so weaker matches only ever append. All SQL is
  parameterized; structured filters from the query language (`query/`)
  are ANDed onto every pass.
- **Baloo compatibility** (`cli.rs`, `mime.rs`): read-only endpoints
  (`status_for_path`, `list_failed`, `index_size_breakdown`, …) and a
  Baloo-shaped type model, groundwork for a future `balooctl`-compatible
  layer.
- **Logging** (`log.rs`): background reporting goes through `log_info!` /
  `log_warn!` rather than `println!`/`eprintln!`. Each writes its line to
  stderr *and* appends it to a bounded in-memory ring (newest 5000 lines,
  with a count of what was dropped) that the GUI's Logs tab reads, so a
  windowed run with no terminal still surfaces them. Command output —
  search hits, usage, the error a command exits with — stays on stdio.
- **Platform differences** (`platform.rs`): the single home for `#[cfg]`.
  Home directory lookup, what counts as a hidden entry (dot-prefix, plus
  the Hidden/System attributes on Windows), network-filesystem detection
  (`/proc/mounts` against `GetDriveTypeW`), path collation, and the
  watch-registration strategy all live here, so the rest of the crate can
  ask a question rather than test a target. Anything decidable from a
  string alone is split out so its tests run on every platform.

### Frontend (`quicksearch-gui`)

Immediate-mode egui/eframe app, one UI thread:

```
UI thread ──SearchRequest──▶ search worker ──SearchUpdate (mpsc)──▶ drained per frame
UI thread ──commands──────▶ IndexCoordinator ──state──▶ polled per frame
core threads ─────────────▶ ctx.request_repaint() (wake the UI)
```

Modules map one-to-one onto what you see: `app.rs` (shell, status bar,
config routing), `search_tab.rs` (query strip, virtualized results table,
snippet highlighting via `LayoutJob` byte ranges, ignore dialog),
`manage_tab.rs` (status detail + `tracker.rs` rate estimation, roots and
filter editors), `duplicates_tab.rs`, `logs_tab.rs` (a virtualized view of
the core log ring), `options.rs` (draft-based settings
editor shared between the window and the Manage tab), `platform.rs`
(open / reveal-in-file-manager, and the Windows stdio setup a
window-subsystem process needs before anything prints), `cli.rs` (terminal
mode, shared with the `quicksearch-cli` binary). There is no
pagination: the table is virtualized, so a single scroll list capped at
`display_limit` renders in microseconds regardless of row count.

## Development

- `cargo test -p quicksearch-core`: unit + integration suites (cascade
  ranking, cancellation, incremental indexing, coordinator modes, config
  resolution, fuzzy matcher vs. brute-force oracle).
- `cargo test -p quicksearch-gui`: formatter/tracker/CLI-parsing units.
- `QSB_SNIPPET_PERF=1 cargo test --release -p quicksearch-core --test
  snippet_perf -- --nocapture`: snippet pipeline benchmark.
- New extractors: implement `extract::Extractor` and register it in
  `Registry::default_set()` — order matters, the first extractor whose
  `supports` accepts a MIME wins. New cascade behavior: `search/cascade.rs`
  documents the rank invariants that keep streamed results append-only.
