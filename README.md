# QuickSearch

A fast local file indexer and search tool. QuickSearch walks your chosen
folders into a compact SQLite index (FTS5 full-text + zstd-compressed text
sidecar), keeps it fresh automatically with filesystem watchers and
periodic reindexing, and serves ranked search-as-you-type results in a
compact egui desktop app, or straight to your terminal.

## AI Disclaimer
QuickSearch has a core designed by it's developer and built by hand, however the majority of it's codebase including it's GUI was designed by a human and built using AI agents with human review, improvements, and testing.

## GitHub Mirror
The primary home of this software is:
https://code.karsttech.com/jeremy/quick_search

The code is also mirrored to GitHub for easier bug reporting and issue tracking:
https://github.com/DataScienceDIY/quick_search

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
`build.sh` also takes `--installer`, which adds NSIS and the mingw-w64 cross
toolchain to what it installs and builds the Windows installer instead of
launching anything — see [Install (Windows)](#install-windows).

Building by hand needs a Rust toolchain plus, on every platform, a C toolchain
and Perl: SQLCipher, zstd and OpenSSL are compiled from bundled C sources, and
OpenSSL's `Configure` is a Perl script. `rust-toolchain.toml` pins the compiler
version and the cross-compilation targets, so rustup installs the right ones on
the first `cargo` command and no `rustup target add` is needed. The old
WebKit/WebView dependencies (`setup.sh`) are gone; the GUI renders with
OpenGL via egui.

- Linux: working OpenGL 3.3 drivers; `xdg-desktop-portal` (present on all
  mainstream desktops) provides the native folder picker. On minimal
  images you may need `build-essential perl pkg-config`. No X11, Wayland or
  xkbcommon `-dev` packages are needed: winit dlopens the display stack at
  run time, so only the runtime libraries matter.
- Windows: Visual Studio 2022 Build Tools with the "Desktop development
  with C++" workload (MSVC v143 plus a Windows SDK), and Perl (Strawberry
  Perl); NASM is optional and only enables OpenSSL's assembly paths. The GNU
  target needs only a mingw-w64 toolchain, and cross-compiles from Linux —
  `cargo build --release -p quicksearch-gui --target x86_64-pc-windows-gnu`
  with `gcc-mingw-w64-x86-64` installed, which is how CI produces the Windows
  binaries. Note that Windows ships only a software OpenGL 1.1
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
sudo apt install ./dist/quicksearch_1.0.2_amd64.deb
```

The script builds the release binary, strips it, and assembles a `.deb` with
`dpkg-deb`. It needs no `cargo-deb`, no `debhelper` and no SVG rasteriser —
only `dpkg-deb` and `desktop-file-utils`, both standard on Debian and Ubuntu.
Useful flags: `--no-build` to package a binary you already built, `--no-strip`
to keep debug symbols, `-o DIR` to write elsewhere. `DEB_MAINTAINER` overrides
the packaging maintainer.

The package installs:

| Path | Contents |
| --- | --- |
| `/usr/bin/quicksearch` | the desktop app, which also does terminal search |
| `/usr/bin/quicksearch-cli` | terminal search only |
| `/usr/share/applications/quicksearch.desktop` | menu entry, so QuickSearch appears in the app launcher |
| `/usr/share/icons/hicolor/{16,22,24,32,48,64,128,256}x*/apps/` | icons at each size |
| `/usr/share/icons/hicolor/scalable/apps/quicksearch.svg` | the source icon |
| `/usr/share/metainfo/com.karsttech.quicksearch.metainfo.xml` | AppStream data, so software centres show a real listing |
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

`quicksearch.ico` in the same directory bundles the 16–256px PNGs unchanged
(one PNG-compressed entry per size) for the Windows installer, which uses it
for the installer window, the shortcuts and the Add/Remove Programs entry.
Regenerate it from the PNGs with Pillow: open each `quicksearch-N.png`,
largest first, and `save(..., format="ICO", sizes=[...], append_images=rest)`
— passing the images rather than one image and a size list is what keeps the
committed pixels instead of resampling them.

## Install (AppImage)

For anything that is not Debian or Ubuntu. Download
`quicksearch-<version>-x86_64.AppImage` from the release page, make it
executable and run it:

```sh
chmod +x quicksearch-1.0.4-x86_64.AppImage
./quicksearch-1.0.4-x86_64.AppImage
```

If it fails to start with a FUSE error — some distributions no longer install
FUSE by default — either install the distribution's FUSE package or run it
unpacked:

```sh
APPIMAGE_EXTRACT_AND_RUN=1 ./quicksearch-1.0.4-x86_64.AppImage
```

To build one, `./packaging/build-appimage.sh` takes the same flags as
`build-deb.sh` (`--no-build`, `--no-strip`, `-o DIR`). It downloads
`appimagetool` and the AppImage runtime, both pinned by sha256 and cached under
`~/.cache/quicksearch`, and needs `zsync` and `appstream` installed for
`zsyncmake` and `appstreamcli`. `APPIMAGETOOL` points it at a copy you already
have. It needs no FUSE itself, which is what lets CI build one in a container.

## Install (Windows)

Download `quicksearch-<version>-windows-x86_64-setup.exe` from the release
page and run it, or build it on a Linux machine:

```sh
./build.sh --installer           # installs the two extra packages first
./packaging/build-installer.sh   # or straight to the build
```

That cross-compiles for `x86_64-pc-windows-gnu` and compiles the installer
with NSIS, which runs on Linux — no Windows machine is involved, and CI
produces the installer in the same job as the `.zip`. It needs `nsis` and
`gcc-mingw-w64-x86-64` (`mingw32-nsis` and `mingw64-gcc` on Fedora, `nsis` and
`mingw-w64-gcc` on Arch; on openSUSE both come from the `windows:mingw` OBS
project, so `build.sh` names them and leaves the repository to you). The same
flags as `build-deb.sh` apply: `--no-build` to package binaries you already
built, `--no-strip`, `-o DIR`; after `--`, `build.sh --installer` passes them
straight through.

The install is per-machine and asks for elevation. Into
`C:\Program Files\QuickSearch` go:

| File | Contents |
| --- | --- |
| `quicksearch.exe` | the desktop app |
| `quicksearch-cli.exe` | terminal search |
| `quicksearch.ico` | icon for the shortcuts and Add/Remove Programs |
| `README.md`, `LICENSE.txt`, `config_example.toml` | documentation |
| `uninstall.exe` | written by the installer; Add/Remove Programs runs it |

The components page offers a Start menu shortcut (on) and a desktop shortcut
(off); both are created for all users. No `config.toml` is installed, for the
same reason the `.deb` ships none — one next to the binaries is portable mode
(see [Configuration](#configuration)) and would override the personal config
of every account. The app writes `%APPDATA%\quicksearch\config.toml` on first
run instead.

Installing over an older version reuses wherever that one went, taken from its
registry entry rather than guessed. Both the installer and the uninstaller
stop with a message if QuickSearch is still running, since Windows will not
replace a running executable and the alternative is a half-replaced install.

Uninstalling removes what was installed and nothing else. The index in
`%LOCALAPPDATA%\quicksearch` and the config in `%APPDATA%\quicksearch` stay,
so reinstalling picks up the existing index; the program directory is removed
only if empty, which leaves a portable-mode `config.toml` and its index alone.

`PATH` is deliberately untouched — add `C:\Program Files\QuickSearch` to it
yourself if you want `quicksearch-cli` on every prompt. It is an NSIS
installer, so it takes `/S` for a silent install and `/D=` for the directory
(last argument, unquoted):

```bat
quicksearch-1.0.2-windows-x86_64-setup.exe /S /D=C:\Tools\QuickSearch
```

The `.zip` on the release page is the alternative to all of this: the same two
binaries, no registry entries and nothing to uninstall. Unpack it anywhere,
and drop a `config.toml` next to the binaries to keep the config and index
inside that folder.

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
percent, files/sec) or the total indexed file count when idle. Applying a
settings change to the index counts as something the indexer is doing: it
reports its progress there and in the Manage Index tab, and says what it
removed for a few seconds after it finishes, so a change that takes a
millisecond is as visible as one that takes minutes.

Quitting while a settings change is still being applied asks first. Leaving
is never refused — the work stops promptly and the index stays consistent —
but it stops part-way, so entries you excluded can still turn up in search
results until indexing runs again. The next launch says so, with a button to
start that run; in automatic mode the periodic reindex does it for you.

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

This protects the index itself and for attacks like data theft.
Anything malicious running with user permissions could bypass this protection,
but anything with user permissions can also access all of the same files.

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
everywhere and additionally anything marked Hidden on Windows, which is
what keeps `AppData`, `$RECYCLE.BIN` and `System Volume Information` out
of the index — the System attribute alone is not enough, because cloud
sync roots carry it purely to get a branded folder icon; and ignore
patterns are matched case-insensitively on Windows and macOS, matching
the filesystem.

**Portable mode**: a `config.toml` sitting next to the `quicksearch`
binary overrides the user config entirely, and relative paths inside any
config resolve against the config file's own directory, so a folder
containing the binary, its config, and its index can be moved wholesale.

The GUI edits the config live; external edits apply on next start.

**Changing what is indexed** does not throw the index away. Narrowing the
scope — removing a folder, adding an ignore pattern, turning off hidden
files or symlink following, shortening `content_extensions` — deletes
exactly the entries that fell out of scope, in place. Widening it — adding
a folder, deleting a pattern, lengthening the extension list — schedules a
reindex to find what is newly in scope. Both happen automatically, in
automatic and manual mode alike, and neither asks first: it is the edit you
just made. Order and spelling are not changes at all, so reordering the
folder list or writing `~/docs` where you wrote `/home/you/docs` costs
nothing.

Only three settings still delete and rebuild the index, because nothing
stored survives them: `processing.tokenize` (part of the FTS table's
definition), `processing.hash_length` (existing hashes become
incomparable), and turning password protection on or off or changing the
password. In manual mode those ask for confirmation first.

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
  may wipe, so it can never destroy an intact index. Each kind of connection
  takes a page cache sized for what it does and how long it lives, rather than
  one figure applied everywhere (`db/schema.rs` sets six profiles and argues
  each): the caches are `malloc`ed, so a connection that scans a table and is
  then held — the coordinator's writer, before it learned to let go when idle —
  keeps that memory for the life of the process. Search is the one deliberately
  large one, because it is the only cache reused often enough to pay for
  itself, and it is released once searching stops.
- **Indexing** (`indexing.rs`, `file_handling.rs`): full runs walk each
  root (`filtered_walk` prunes hidden/ignored subtrees before descending),
  classify files by mtime into insert/update/skip, batch-write metadata,
  sweep stale rows, then extract content (plaintext, RTF, Office — both the
  OOXML/ODF zip formats and the pre-2007 binary `.doc`/`.xls`/`.ppt`, whose
  OLE2 streams are read in `extract/ole.rs` — PDF, audio tags, EXIF; see
  `extract/`) for FTS. PDFs are parsed once, with the text and the `Info`
  dictionary taken off the same document: the two-parse version that preceded
  it was a run's largest single memory consumer, and it was what pulled a
  second copy of `lopdf` — and with it rayon's never-torn-down thread pool —
  into the build. Files whose extension no MIME
  table knows — including extensionless ones like `README` or `Makefile` —
  are sniffed from their head bytes and indexed as text only when that head
  is provably text: valid UTF-8, or BOM-marked (`mime.rs`, `textenc.rs`).
  Legacy charsets are decoded via chardetng and stored as UTF-8, but only
  for files something *else* typed as text, normally their extension —
  chardetng's windows-1252 floor never fails, so accepting it on a bare
  sniff would adopt any binary lacking NUL bytes. More claimed files means a
  bigger index — `indexing.content_extensions` remains the throttle. Files no larger than `processing.hash_length` skip that second
  pass entirely: the head the walk reads to hash them is already their
  whole content, so a plaintext body is extracted in the same `read` and
  stored complete. Every run ends — whether
  it completed or was stopped — with an optimize pass on its own connection:
  checkpoint, VACUUM if the file has at least 10% slack to reclaim, `PRAGMA
  optimize`, checkpoint again. Progress streams through a polled
  `IndexingStatus`, which reads `Optimizing` for the duration of that pass —
  and `Preparing` for everything a run does before its first file is walked:
  waiting on the previous run's thread, opening the index (a WAL recovery
  lands here), and reconciling a changed configuration. Each carries the run's
  start time, so a prologue that outlasts the walk on a large index reads as
  slow work rather than a hang.
- **Scope reconciliation** (`scope.rs`): the index is a cache of what a walk
  under the configured roots would produce, so a configuration change is a
  difference between the two rather than a reason to start over.
  `config::diff_actions` turns old-versus-new into an `IndexWork` plan —
  roots to delete by path range, rows to re-test against the walker's own
  filtering rules (`Scope::covers` mirrors `read_directory` exactly, or the
  next run would re-add what the last prune removed), stored text to
  re-decide, and whether a walk must follow. The coordinator applies it in
  250 ms slices so a multi-million-row scan never blocks its command loop,
  and every run applies it once more against the `config_validation`
  fingerprint, which is what makes a config hand-edited while the app was
  closed behave like one edited live. The scan is per-root, by `[lo, hi)`
  range: a symlink target stored outside every root has no owning root and
  therefore no rules that could be applied to it, so it is never visited.
  Whichever of the two applies it, a pass that *finishes* records what it
  reconciled against — everything but the three rebuild-only keys, which no
  scan can satisfy. That record is the whole convergence condition: an
  abandoned pass leaves it alone and the next run picks the work back up,
  while a completed one stops every later run from re-deriving the same plan
  and rescanning every row to redo work already done. Both report a live
  `ReconcileProgress` while they scan, since on a large index this is minutes
  of work with no files moving to show for it. Both can also be abandoned:
  `advance` reads a cancel flag before every statement, and the statement
  already running — one `DELETE` can cover a whole root — is ended by
  `sqlite3_interrupt`, since a flag alone cannot reach inside SQLite. That is
  what makes closing the window during a prune immediate instead of a wait
  the desktop offers to kill. `scope::outstanding_work` asks the record what
  is still owed, which is how the GUI knows to remind you at the next launch.
- **Coordinator** (`coordinator.rs`): the object binaries construct.
  Owns the `IndexingService`, the debouncing filesystem watcher
  (`watcher.rs`), and the mode state machine (Auto / Manual, persisted as
  `indexing.auto_index` — the mode the app is left in is the mode it
  starts in, and a config carrying a different value switches it). Watcher
  events become single-file transactions (`incremental.rs`) that keep
  `files`, FTS, and the text sidecar consistent per commit; a full
  reindex runs on a configurable interval. Incremental writes defer while
  a full run is active, so there is exactly one writer at a time — scope
  reconciliation defers with them, for the same reason.
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
  so typing never waits. The worker keeps its connection across requests and
  drops it once searching stops, so a typing session runs against a page cache
  that is already warm instead of rebuilding one per keystroke; because a
  rebuild or clear puts a *new* file at the *same* path, an index generation
  counter (`db::index_epoch`) is what tells the held connection to reopen. The cascade streams rank-ordered batches: one
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
- **Baloo compatibility** (`cli.rs`, `mime.rs`): the read API this repo's
  parent consumes — `status_for_path`, `list_failed`,
  `index_size_breakdown`, `pending_content_count`, `clear_path` — plus a
  Baloo-shaped type model. Only `index_counts` has a caller inside this
  repository; the rest are a compatibility surface for the parent's
  `balooctl` layer and are not dead code.
- **Logging** (`log.rs`): background reporting goes through `log_info!` /
  `log_warn!` rather than `println!`/`eprintln!`. Each writes its line to
  stderr *and* appends it to a bounded in-memory ring (newest 5000 lines,
  with a count of what was dropped) that the GUI's Logs tab reads, so a
  windowed run with no terminal still surfaces them. Command output —
  search hits, usage, the error a command exits with — stays on stdio.
- **Platform differences** (`platform.rs`): the single home for `#[cfg]`.
  Home directory lookup, what counts as a hidden entry (dot-prefix, plus
  the Hidden attribute on Windows — System deliberately excluded, since
  cloud sync roots set it to get a folder icon), network-filesystem detection
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
- `cargo test -p quicksearch-gui`: formatter/tracker/CLI-parsing units plus
  headless egui tests that drive the real widgets — building an input frame,
  synthesizing clicks and reading back the painted text (`test_ui.rs`) — over
  the search and manage tabs, the options editor, the unlock gate, the logs
  and duplicates tabs, and query highlighting.
- `QSB_SNIPPET_PERF=1 cargo test --release -p quicksearch-core --test
  snippet_perf -- --nocapture`: snippet pipeline benchmark.
- `QSB_SEARCH_PERF=1 cargo test --release -p quicksearch-core --test
  search_perf -- --nocapture`: what a warm page cache is worth to search, swept
  across cache ceilings and run both encrypted and not. It exists because the
  right size for `PRAGMAS_SEARCH` is not something to reason about: encrypted,
  the curve has a cliff at the working set, because SQLCipher caches pages
  decrypted and a miss below that costs an AES-CBC plus an HMAC-SHA512 per
  4 KiB page. Unencrypted it is flat. Read it before changing that number.
- Memory probes, all under `crates/quicksearch-core/examples/`:
  `memprobe <cold|warm> <root> <db>` reports an indexing run's peak *and* what
  it settles at once idle — the gap between those is the memory a process
  keeps for nothing, since glibc's `free` returns chunks to its arena rather
  than to the kernel. `rssprobe <pid> [duration_s]` attributes a *running*
  process's footprint instead, splitting anonymous heap (ours) from
  file-backed pages (the binary, libc, the GL stack), reading `Private_Dirty`
  rather than `VmRSS`, and counting glibc's arenas so retention is
  distinguishable from live data. It reads another process's `/proc`, so it
  measures a build made without knowing it would be measured.
  `indexprobe` and `walkprobe` answer "how fast" rather than "how much".
- `.forgejo/workflows/ci.yml`: builds both platforms on every push to `master`
  and every pull request. To cut a release, bump `[workspace.package] version`
  in `Cargo.toml` and push the commit on a branch named `Release...`; CI runs
  `cargo update -w` first, so a lockfile still pinning the old member versions
  is not something you have to remember. That only re-resolves the workspace
  crates, so the `--locked` build after it still fails on a dependency added or
  bumped without committing `Cargo.lock`. Once both
  build jobs are green, CI tags that commit `v<version>` and publishes a release
  with the `.deb`, an AppImage and its `.zsync` sidecar, a Linux tarball, the
  Windows installer and a Windows zip attached; pushing a `v*` tag by hand does
  the same thing. The sidecar is the one asset named without a version, because
  every released AppImage embeds its URL and that URL has to keep resolving as
  releases come and go — Forgejo resolves the literal tag `latest` to the newest
  release and looks an asset up by name, so it is always at
  `.../releases/download/latest/quicksearch-x86_64.AppImage.zsync`. Note that is
  `/releases/download/latest/`, not the GitHub-style `/releases/latest/download/`,
  which Forgejo does not implement. The version is never taken from the branch
  name, and a tag that already exists at a different commit aborts the release
  rather than shipping two builds under one version. Every build carries its
  identity: `crates/quicksearch-gui/build.rs` bakes in the commit CI passes as
  `QS_COMMIT`, and the pair shows up as `v<version> (<commit>)` in the
  bottom-right of the status bar, from `quicksearch-cli --version`, and in the
  Windows `.exe` properties. A build made outside a git checkout reads
  `unknown` there rather than failing. The Linux job runs in an Ubuntu
  22.04 container on purpose — `packaging/build-deb.sh` reads the package's
  `libc6` floor from the binary it just built, so the builder's glibc becomes
  the package's minimum, and 22.04 pins it at 2.35. The AppImage is cut from
  that same binary and bundles no libraries, so 2.35 is its floor too — it is
  the one number that decides how far either Linux artifact reaches.
  The Windows job
  cross-compiles with mingw-w64 and fails if either `.exe` picks up a
  dependency on a non-system DLL, then builds both Windows assets from those
  binaries — `packaging/build-installer.sh` runs `makensis`, which is a Linux
  program, so the installer needs no Windows runner either.
- New extractors: implement `extract::Extractor` and register it in
  `Registry::default_set()` — order matters, the first extractor whose
  `supports` accepts a MIME wins. New cascade behavior: `search/cascade.rs`
  documents the rank invariants that keep streamed results append-only.
- `packaging/capture.sh`: regenerates the website assets — `search.webm`,
  `manage-indexing.webm`, `duplicates.png`, `query-highlight.png` — into
  `packaging/captures/` (gitignored). It builds the GUI with the `capture`
  feature, whose scripted driver types, switches tabs, waits on indexer
  state, and captures both screenshots and video frames from the app's own
  framebuffer (piped to ffmpeg), so the display server never matters — X11
  and Wayland record identically, and overlapping windows can't leak into
  the footage; `packaging/capture-scenario.txt` is the choreography and is
  meant to be edited. Runs against a throwaway index of this repository plus
  `~/.cargo/registry/src` under scratch XDG dirs, so your real config and
  index are untouched. Needs a graphical session and ffmpeg with
  `libx264rgb` and `libvpx-vp9`.
