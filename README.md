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

These take a fresh machine to a running app: install missing build
dependencies, build release, launch the GUI. Stages already satisfied are
skipped, so a normal run costs one `cargo build`. `build.sh` installs
system packages via `sudo` (apt/dnf/pacman/zypper) and the Rust toolchain
with rustup; `build.bat` uses winget and rustup. Both take `--check` to
report dependency status without installing or building, `--no-run` to
stop after the build, and `--` to pass the rest to the binary. `build.sh`
also takes `--installer`, which builds the Windows installer instead of
launching anything — see [Install (Windows)](#install-windows).

Building by hand needs a Rust toolchain plus, on every platform, a C
toolchain and Perl: SQLCipher, zstd and OpenSSL are compiled from bundled
C sources, and OpenSSL's `Configure` is a Perl script.
`rust-toolchain.toml` pins the compiler version and the cross-compilation
targets, so rustup installs the right ones on the first `cargo` command.

- Linux: working OpenGL 3.3 drivers; `xdg-desktop-portal` (present on all
  mainstream desktops) provides the native folder picker. On minimal
  images you may need `build-essential perl pkg-config`. No X11, Wayland or
  xkbcommon `-dev` packages are needed: winit dlopens the display stack at
  run time, so only the runtime libraries matter.
- Windows: Visual Studio 2022 Build Tools with the "Desktop development
  with C++" workload (MSVC v143 plus a Windows SDK), and Strawberry Perl;
  NASM is optional. The GNU target needs only a mingw-w64 toolchain and
  cross-compiles from Linux (`cargo build --release -p quicksearch-gui
  --target x86_64-pc-windows-gnu`), which is how CI produces the Windows
  binaries. Windows ships only a software OpenGL 1.1 driver, so a bare VM
  or an RDP session without a vendor GPU driver cannot create a context
  and the window will fail to open.
- macOS: Xcode command line tools (`build.sh` does not auto-install these).

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
sudo apt install ./dist/quicksearch_<version>_amd64.deb
```

Substitute the current release version for `<version>`.

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
PNGs rasterised from it. The PNGs are committed, so an ordinary `cargo
build` needs no image tooling — the 256px one is compiled into the binary
with `include_bytes!` and becomes the window icon. Editing the SVG means
re-rendering the PNGs; `packaging/build-deb.sh` documents how in a comment
at the top.

X11 takes the window icon from the embedded PNG. Wayland ignores it and
matches the app id (`quicksearch`) against the installed
`quicksearch.desktop`, so there the titlebar icon appears only once the
package is installed. `quicksearch.ico` bundles the 16–256px PNGs
unchanged for the Windows installer; regenerate it from the PNGs with
Pillow (`append_images` keeps the committed pixels unresampled).

## Install (AppImage)

For anything that is not Debian or Ubuntu. Download
`quicksearch-<version>-x86_64.AppImage` from the release page (substitute
the current release version), make it executable and run it:

```sh
chmod +x quicksearch-<version>-x86_64.AppImage
./quicksearch-<version>-x86_64.AppImage
```

If it fails to start with a FUSE error — some distributions no longer install
FUSE by default — either install the distribution's FUSE package or run it
unpacked:

```sh
APPIMAGE_EXTRACT_AND_RUN=1 ./quicksearch-<version>-x86_64.AppImage
```

To build one, `./packaging/build-appimage.sh` takes the same flags as
`build-deb.sh` (`--no-build`, `--no-strip`, `-o DIR`). It downloads
`appimagetool` and the AppImage runtime, both pinned by sha256 and cached under
`~/.cache/quicksearch`, and needs `zsync` and `appstream` installed for
`zsyncmake` and `appstreamcli`. `APPIMAGETOOL` points it at a copy you already
have. It needs no FUSE itself, which is what lets CI build one in a container.

## Install (Windows)

Download `quicksearch-<version>-windows-x86_64-setup.exe` from the release
page (substitute the current release version) and run it, or build it on a
Linux machine:

```sh
./build.sh --installer           # installs the two extra packages first
./packaging/build-installer.sh   # or straight to the build
```

That cross-compiles for `x86_64-pc-windows-gnu` and compiles the installer
with NSIS, which runs on Linux — no Windows machine is involved. It needs
`nsis` and `gcc-mingw-w64-x86-64` (`mingw32-nsis` and `mingw64-gcc` on
Fedora, `nsis` and `mingw-w64-gcc` on Arch; on openSUSE both come from the
`windows:mingw` OBS project). The same flags as `build-deb.sh` apply:
`--no-build`, `--no-strip`, `-o DIR`; after `--`, `build.sh --installer`
passes them straight through.

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
(off); both are created for all users. No `config.toml` is installed — one
next to the binaries is portable mode (see [Configuration](#configuration))
and would override the personal config of every account; the app writes
`%APPDATA%\quicksearch\config.toml` on first run instead. Installing over
an older version reuses that version's install directory, taken from its
registry entry, and both the installer and the uninstaller stop with a
message if QuickSearch is still running.

Uninstalling removes what was installed and nothing else. The index in
`%LOCALAPPDATA%\quicksearch` and the config in `%APPDATA%\quicksearch` stay,
so reinstalling picks up the existing index; the program directory is removed
only if empty, which leaves a portable-mode `config.toml` and its index alone.

`PATH` is untouched — add `C:\Program Files\QuickSearch` to it yourself if
you want `quicksearch-cli` on every prompt. It is an NSIS installer, so it
takes `/S` for a silent install and `/D=` for the directory (last argument,
unquoted):

```bat
quicksearch-<version>-windows-x86_64-setup.exe /S /D=C:\Tools\QuickSearch
```

The `.zip` on the release page is the alternative to all of this: the same two
binaries, no registry entries and nothing to uninstall. Unpack it anywhere,
and drop a `config.toml` next to the binaries to keep the config and index
inside that folder.

## Usage

### GUI

`quicksearch` with no query arguments opens the app. **One window at a
time**: a second launch reports that QuickSearch is already running and
exits, because two processes indexing one database corrupt it. Terminal
search (`quicksearch <query>`) only reads and keeps working while the
window is open. The guard is a kernel lock on `<database_path>.lock`, so a
crash or a power cut releases it and the leftover file never locks you
out. The lock follows `database_path`: point Settings at a different index
and it moves with you. An index locked by another instance, or a SQLite
database belonging to some other program, is refused with an error rather
than written; the file there is created when missing and replaced only
when it is an index from an older layout of QuickSearch's own.

- **Search**: results appear as you type; every keystroke cancels the
  previous search. One checkbox enables the two fuzzy passes, and once a
  search has finished a button inside the right of the search box re-runs
  it. Click a column heading to sort by it; **right-click any heading to
  choose which columns are shown** — the path is always there, and size
  and modified date start hidden. The choice is saved (`[search.columns]`,
  also in Settings → Search) and applies immediately. Sorting by a column
  you then hide falls back to rank. Double-click a result to open it;
  right-click it to reveal it in the file manager, open it, copy its
  path, or build an ignore filter from it (session-only by default,
  optionally persisted to the config). Result text can be selected and
  copied in place. A match in a file's **name** or **path** is
  highlighted in that column; a match in its **contents** shows a
  highlighted snippet in the Content Match column, with more of the
  surrounding text on hover. Rows matched on name or path show a dash
  there instead. With `[search] live_results` on (the default) the rows
  on screen are watched and checked against the disk, so a rename,
  deletion or edit shows within a second whether or not indexing is
  running — see `config_example.toml`.
- **Manage Index**: full indexing status, Start/Stop/Automatic controls,
  indexed folder list, full-text extension filters, ignore patterns, and
  the indexing options. Stopping switches to manual mode and saves that
  (`indexing.auto_index`), so a stopped index stays stopped across
  restarts until you return to automatic. The index size beside the
  status heading totals the database and its `-wal`/`-shm` sidecars,
  refreshed every ten seconds; hovering it lists the ways to make it
  smaller. Each folder in the list shows how many files it holds and how
  many of those had text extracted, counted as each indexing run finishes
  and stored with the index; a folder nothing has finished indexing reads
  "not yet indexed".
- **Duplicates**: files sharing a content hash, grouped. The hash covers
  each file's size and its first `processing.hash_length` bytes and
  nothing else, so a group is a strong suspicion, not a certainty.
  Right-click a group, or any file in one, to settle it: every member is
  read through and compared byte for byte, with progress and a Cancel
  button in a modal that then names each file as identical, differing at
  a given byte, a different size, or unreadable. Nothing is deleted or
  changed either way; the point is to know before you delete something
  yourself. **Sort by** lists the groups either by reclaimable space or by
  file extension, for working through one file type at a time. It reorders
  what the scan already returned rather than asking for a different set, so
  which groups are listed never changes: they are always the 500 wasting the
  most space, said as much in the line above the list. The scan runs on the
  first visit to the tab and the listing then stays put, so coming back to it
  is instant; **Refresh** re-runs it, and so does anything that moves the
  index underneath it — a finished indexing run, or pointing the app at a
  different index.
- **Logs**: the lines the app would have printed to a terminal — warnings
  from indexing, folder watching and opening files, newest last, with a
  filter box and Copy button. Launched from a desktop launcher (or on
  Windows, where the app has no console at all) this is the only place
  they are visible.
- **Help**: an in-app quickstart — first indexing run, example queries,
  what each tab does — pointing here for everything technical. A brand-new
  installation is shown a short click-through introduction covering the
  same ground on its first launch; the Help tab brings it back. Upgrading
  into this version does not raise it (see `[ui] tutorial_seen`). The
  introduction walks the app rather than describing it: each page switches
  to the tab it is about, colours the keywords in its own prose, and pulses
  the widget each keyword names in the same colour — the query box, the
  Rank column, the status bar, the Fuzzy tick, a tab. One page types a
  search into the box to show results arriving. Its window is draggable and
  not modal, so it can be moved off whatever it is pointing at and the app
  used underneath it.
- **Settings**: every configuration control in one place — the database
  path, indexing and processing limits, search behaviour, the interface
  (scale, shortcut, color scheme) and password protection. Each row
  explains itself on hover. Edits are staged and applied together by
  **Apply & Save**; leaving the tab with unapplied edits asks first. The
  column choices and the password controls are the exceptions, acting the
  moment they are used. The indexed folder list and the indexing mode
  live on Manage Index instead, next to the controls that act on them.

**Ctrl+Shift+F from anywhere** brings QuickSearch to the front, restoring
it if it was minimized, and puts the cursor in the search box with the
previous search selected. The Settings tab's Interface section rebinds it
— click the button and press the keys — or switches it off. It is a
system-wide shortcut, registered with Windows or with the X server, so it
works while another application has focus. Wayland does not let an
application claim a key, so there the shortcut is registered with your
desktop through the XDG desktop portal; your desktop then has the final
say over which key it is, and the Settings tab says which key it settled
on. Wayland likewise gives no application a way to raise itself, so under
it the shortcut selects the Search tab and the search box but leaves
raising the window to the desktop; on X11 and Windows it raises and
restores the window itself.

The bottom status bar always shows what the indexer is doing (phase,
percent, files/sec) or the total indexed file count when idle. Applying a
settings change to the index counts as something the indexer is doing: it
reports its progress there and in the Manage Index tab, and says what it
removed for a few seconds after it finishes.

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
concentrated risk in one file. **Settings → Security → Enable password
protection** encrypts the index on disk with SQLCipher; from then on
QuickSearch asks for the password every time it starts, in the GUI (an
unlock screen before anything opens the index) and in the terminal (a
hidden prompt). Enabling, disabling, or changing the password deletes and
rebuilds the index — there is no in-place conversion.

- The key is derived as `Argon2id(password, salt)`; the salt is written to
  `config.toml` when the password is set (it is unique, not secret, and
  required — keep it with the config if you copy a protected setup).
- Pages are AES-256-CBC at an 8192-byte page size, with SQLCipher's
  per-page HMAC **deliberately disabled** — it only detects tampering by
  someone who could already read the indexed files directly, and it costs
  1.78x on search. Confidentiality is unchanged.
- **Remember on this device** stores the derived key (never the password)
  in the OS keychain — Secret Service/KWallet on Linux, Credential Manager
  on Windows — and skips the prompt. Without a keychain daemon the option
  quietly falls back to prompting.
- **Show database key** asks for the password, then shows the raw SQLCipher
  key as `0x…` (64 hex digits) with a copy button, alongside the page size
  and HMAC setting another tool has to be given — on SQLCipher's defaults
  the index decrypts to noise and every tool calls a correct key wrong.
  That key alone reads the index, so treat a copy of it as carefully as
  the password.
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
(Windows: `%LOCALAPPDATA%\quicksearch\index.sqlite`).

`config_example.toml` is the full reference — every key with its default,
valid range, and caveats. It ships in the repository root, in the `.deb`
under `/usr/share/doc/quicksearch/`, and in the Windows install
directory. In brief: `[paths]` says what to index and where the index
lives; `[indexing]` sets the mode, reindex interval, symlink and
hidden-file policy, `content_extensions` and `ignore_patterns`;
`[processing]` sets extraction and storage limits (`hash_length`, text
size caps, batching, `maximum_wal_size`, `tokenize`,
`store_text_for_snippets`); `[security]` covers password protection;
`[ui]` covers scale, `search_hotkey` and `color_scheme`; `[search]`
covers the fuzzy settings, result limits, `live_results` and the visible
columns.

The GUI edits the config live; external edits apply on next start. A
hand-edited value outside a setting's working range is clamped with a
warning, never rejected — a typo in a text file must not stop the app
starting.

**Portable mode**: a `config.toml` sitting next to the `quicksearch`
binary overrides the user config entirely, and relative paths inside any
config resolve against the config file's own directory, so a folder
containing the binary, its config, and its index can be moved wholesale.

**Changing what is indexed** does not throw the index away: narrowing the
scope deletes exactly the entries that fell out of scope, in place, and
widening it schedules a reindex to find what is newly in scope — both
automatically, in automatic and manual mode alike. Only three settings
still delete and rebuild the index, because nothing stored survives them:
`processing.tokenize`, `processing.hash_length`, and turning password
protection on or off or changing the password. In manual mode those ask
for confirmation first.

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
  `wal_checkpoint(TRUNCATE)` every `processing.maximum_wal_size` bytes,
  checkpoints sooner when free space is short, and stops with an error
  before the disk fills (see `config_example.toml`). The index's own
  files are never walked into: opening one to hash it would cancel the
  process's POSIX advisory locks on that inode, SQLite's documented
  corruption hazard. `files` holds metadata (name, path, size, mtime,
  hash, MIME/type bitmask, content state); `searchabletext` is a
  *contentless* FTS5 table over the document body (postings only,
  trigram tokenizer by default) — filename ranks come from scanning
  `files.name`; canonical extracted text lives zstd-compressed in
  `documents_text`, powering snippets, occurrence ranking, and fuzzy
  full-text search. Schema changes wipe and rebuild by policy: only the
  indexer (`open_or_recreate`) may do that, and every consumer uses
  `open_existing`, which treats drift as an error, never data loss. With
  password protection on, every open applies the Argon2id-derived raw key
  (`security.rs`, process-global in `db/key.rs`) first; a wrong key is a
  tagged `KEY_MISMATCH` error, structurally distinct from schema drift,
  so it can never destroy an intact index. Each connection kind takes a
  page cache sized for its job and lifetime — `db/schema.rs` sets six
  profiles and explains each — released, heap included
  (`platform::release_free_heap`), when the search worker or idle writer
  lets go.
- **Indexing** (`indexing.rs`, `file_handling.rs`): full runs walk each
  root (`filtered_walk` prunes hidden/ignored subtrees before
  descending), classify files by mtime into insert/update/skip,
  batch-write metadata, sweep stale rows, then extract content for FTS —
  plaintext, RTF, Office (the OOXML/ODF zip formats and the pre-2007
  binary OLE2 formats, `extract/ole.rs`), PDF (parsed once per file),
  audio tags; see `extract/`. Images are claimed by no extractor, which
  keeps the content pass from opening every image on disk. Files whose
  extension no MIME table knows — including extensionless ones like
  `README` — are sniffed from their head bytes and indexed as text only
  when that head is provably text: valid UTF-8, or BOM-marked
  (`mime.rs`, `textenc.rs`). Legacy charsets are decoded via chardetng
  and stored as UTF-8, but only for files something *else* typed as text
  (a bare sniff would adopt any binary lacking NUL bytes);
  `indexing.content_extensions` is the throttle. Files no larger than
  `processing.hash_length` skip the content pass: the head the walk
  reads to hash them is already their whole content. Every root runs its
  own walker and extraction pools, but all writes go through one thread
  and one connection (`indexing/pipeline.rs`) — the thread where FTS5
  tokenizes, the run's dominant cost. Its loop keeps the walk first:
  each round serves every walking root, then one extracting root, and no
  turn runs past a 100 ms slice, so one root's big documents never park
  another root's walkers. Every run ends — completed or stopped — with
  an optimize pass: checkpoint, VACUUM if the file has at least 20%
  slack, `PRAGMA optimize`, checkpoint again. Progress streams through a
  polled `IndexingStatus` (`Optimizing` during that pass, `Preparing`
  for everything before the first file is walked). The upkeep a run does
  *between* files — WAL checkpoints, the stale-row sweep, FTS merges,
  the per-root recount — blocks the writer for as long as it takes, so
  each announces itself as a `MaintenanceStep` on the published run
  rather than leaving the per-file counters frozen and reading as a
  hang.
- **Scope reconciliation** (`scope.rs`): the index is a cache of what a
  walk under the configured roots would produce, so a configuration
  change is a difference between the two, not a reason to start over.
  `config::diff_actions` turns old-versus-new into an `IndexWork` plan:
  roots to delete by path range, rows to re-test against the walker's
  own rules (`Scope::covers` mirrors `read_directory` exactly), stored
  text to re-decide, and whether a walk must follow. The coordinator
  applies it in 250 ms slices; every run applies it once more against
  the `config_validation` fingerprint, so a config hand-edited while the
  app was closed behaves like one edited live. A finished pass records
  what it reconciled against; an abandoned one leaves the record so the
  next run resumes the work, a completed one stops later runs from
  re-deriving the plan, and `scope::outstanding_work` is how the GUI
  knows to remind you at the next launch. Both paths report live
  progress and can be abandoned mid-statement (`sqlite3_interrupt`).
- **Coordinator** (`coordinator.rs`): the object binaries construct.
  Owns the `IndexingService`, the debouncing filesystem watcher
  (`watcher.rs`), and the Auto/Manual mode state machine (persisted as
  `indexing.auto_index`). Watcher events become single-file transactions
  (`incremental.rs`) that keep `files`, FTS, and the text sidecar
  consistent per commit; a full reindex runs on a configurable interval.
  Incremental writes and scope reconciliation defer while a full run is
  active, so there is exactly one writer at a time. Watch registration
  follows the platform: inotify takes one watch per surviving directory
  (skipping `.git`, `node_modules` and hidden subtrees, which keeps the
  count affordable); `ReadDirectoryChangesW` covers a whole tree from
  one handle. Either way a tree too large to watch degrades to periodic
  reindexing instead of going silently stale.
- **Live results** (`live.rs`): a second, much smaller watcher, owned by
  the frontend, pointed at the parent directories of the result rows
  currently on screen — directories, not files, because editors save by
  renaming a temporary file over the target. What a row shows is read
  from the **file**, never from the index (metadata from `stat`, content
  re-extracted and re-cut through the same `cascade::text_snippet` the
  search uses), which is what makes it work with indexing stopped.
  Arming also checks each row once against the disk, and the paths just
  read go to `IndexCoordinator::update_paths`, applied on the
  coordinator's own thread — the single-writer rule stays intact. Caps
  at 64 directories and 256 rows, rate-limited per path, and dropped
  wholesale the moment the query is edited.
- **Search** (`search/`): `SearchService` runs one worker thread; each
  query is a *generation*. New queries interrupt the in-flight SQLite
  statement (`InterruptHandle`) and stale generations stop cooperatively,
  so typing never waits. The worker keeps its connection across requests
  and drops it once searching stops, so a typing session runs against a
  warm page cache; an index generation counter (`db::index_epoch`) tells
  the held connection to reopen when a rebuild puts a new file at the
  same path. The cascade streams rank-ordered batches: one `files` scan
  classifies exact/case/substring filename matches (ranks 1–4) and sets
  aside full-path matches from the same rows (ranks 9–10); one FTS
  phrase probe verified against the decompressed text yields full-text
  ranks 5–6 ordered by occurrence count; the opt-in fuzzy passes run a
  bitap (Wu–Manber) matcher over filenames (rank 7), document text
  (rank 8) and paths (rank 11). The deferred path tiers flush last, so
  weaker matches only ever append. All SQL is parameterized; structured
  filters from the query language (`query/`) are ANDed onto every pass.
  The passes that read document text share one reusable decompressor and
  output buffer per scan — see `DocDecoder` in `db/repo.rs` for the
  allocation rules that keep that path cheap.
- **Duplicate listing** (`search/duplicates.rs`): three queries,first a  
  sorted `idx_files_hash`, iterating the sorted hashes to find duplicates,
  then hydrating them with size and path information, keeping the largest.
- **Duplicate verification** (`verify.rs`): the second opinion on a group
  from `search/duplicates.rs`, which groups by `sha256(size ‖ head)` and
  so cannot tell apart files that differ only past the head. One
  lockstep byte-for-byte pass — not a hash; the head hash already gave
  the probabilistic answer. Members whose length disagrees are dropped
  unread; the rest are compared span by span against the first member
  that *opened*, so one unreadable file costs its own verdict and nobody
  else's, and a file truncated mid-run degrades to a short comparison.
  The read buffers share a fixed 8 MiB however many members a group has.
- **Baloo compatibility** (`cli.rs`, `mime.rs`): the read API this repo's
  parent consumes — `status_for_path`, `list_failed`,
  `index_size_breakdown`, `pending_content_count`, `clear_path` — plus a
  Baloo-shaped type model. Only `index_counts` has a caller inside this
  repository; the rest are a compatibility surface for the parent's
  `balooctl` layer and are not dead code.
- **Logging** (`log.rs`): background reporting goes through `log_info!` /
  `log_warn!`. Each writes its line to
  stderr *and* appends it to a bounded in-memory ring (newest 5000 lines,
  with a count of what was dropped) that the GUI's Logs tab reads, so a
  windowed run with no terminal still surfaces them. Command output —
  search hits, usage, the error a command exits with — stays on stdio.
- **Platform differences** (`platform.rs`): the single home for `#[cfg]`.
  Home directory lookup, what counts as a hidden entry (dot-prefix, plus
  the Hidden attribute on Windows — System deliberately excluded, since
  cloud sync roots set it to get a folder icon), network-filesystem
  detection (`/proc/mounts` against `GetDriveTypeW`), path collation, and
  the watch-registration strategy all live here, so the rest of the crate
  asks questions instead of testing `cfg` targets. Anything decidable from a
  string alone is split out so its tests run on every platform.

### Frontend (`quicksearch-gui`)

Immediate-mode egui/eframe app, one UI thread:

```
UI thread ──SearchRequest──▶ search worker ──SearchUpdate (mpsc)──▶ drained per frame
UI thread ──commands──────▶ IndexCoordinator ──state──▶ polled per frame
core threads ─────────────▶ ctx.request_repaint() (wake the UI)
```

Modules map one-to-one onto what you see: `app.rs` (shell and config
routing, with `app/` submodules for the status bar, the security flow, the
confirmation modals and the duplicate-verification modal — the one place a
worker's progress is shown in a window, not the status bar),
`search_tab.rs` (query strip and virtualized
results table; snippet rendering via `LayoutJob` byte ranges, the ignore
dialog and the syntax help live in `search_tab/`), `manage_tab.rs` (status
detail + `tracker.rs` rate estimation, roots and filter editors),
`duplicates_tab.rs`, `logs_tab.rs` (a virtualized view of the core log
ring), `settings_tab.rs` (the draft-based config editor, the second of the
two tabs that stage their edits behind an Apply & Save), `tutorial.rs` (the
first-start tour) with `spotlight.rs` (the pass-scoped registry of where the
widgets it points at were drawn — written by the tabs as they lay out, read
by the tour at the end of the same pass, and inert whenever the tour is
closed), `platform.rs`
(open / reveal-in-file-manager, and the
Windows stdio setup a window-subsystem process needs before anything
prints), `hotkey/` (the system-wide search shortcut: one key table feeding
both a `RegisterHotKey` / `XGrabKey` registration and, on Wayland, an XDG
portal session on its own thread), `cli.rs` (terminal mode, shared with the
`quicksearch-cli` binary). There is no pagination: the table is
virtualized, so a single scroll list capped at `display_limit` renders in
microseconds regardless of row count.

## Development

- `cargo test -p quicksearch-core`: unit + integration suites — cascade
  ranking, cancellation, incremental indexing, coordinator modes, config
  resolution, fuzzy matcher vs. brute-force oracle, `verify.rs`'s
  byte-for-byte comparison, and `live.rs`'s event classification (the
  platform-specific rename and atomic-save shapes are synthesized, so
  they are checked on every platform).
- `cargo test -p quicksearch-gui`: formatter/tracker/CLI-parsing units
  plus headless egui tests that drive the real widgets — building an
  input frame, synthesizing clicks and reading back the painted text
  (`test_ui.rs`) — over the search, manage and settings tabs, the unlock
  gate, the logs and duplicates tabs, the first-start tour, and query
  highlighting.
- `cargo bench -p quicksearch-core --bench search` and `--bench index`:
  divan microbenchmarks over the two hot paths, A/B-ing the current code
  against a considered change on one seeded corpus (`benches/corpus/`).
  This is the harness to extend when a hot path is in question.
- `QSB_SEARCH_PERF=1 cargo bench -p quicksearch-core --bench search_perf`:
  what a warm page cache is worth to search, swept across cache ceilings,
  encrypted and not. Read it before changing `PRAGMAS_SEARCH`.
- `QSB_SEARCH_ALLOC=1 cargo bench -p quicksearch-core --bench search_alloc`:
  what a search moves through the allocator, per query shape.
- Memory probes, all under `crates/quicksearch-core/examples/`:
  `memprobe <cold|warm> <root> <db>` reports an indexing run's peak and
  what it settles at once idle; `rssprobe <pid> [duration_s]` attributes
  a running process's footprint, distinguishing retention from live data;
  `indexprobe` and `walkprobe` answer "how fast", not "how much".
  All of them read the resident set and none can see allocator churn, so
  measure allocations separately before concluding a path is cheap.
- `.forgejo/workflows/ci.yml`: builds and tests both platforms on every
  push to `master` and every pull request; packaging and release
  publication run only on a `v*` tag or a `Release...` branch. The
  release mechanics — version guards, tagging, asset naming, the `.zsync`
  update URL — are documented in that file and in
  `packaging/build-appimage.sh`.
- New extractors: implement `extract::Extractor` and register it in
  `Registry::default_set()` — order matters, the first extractor whose
  `supports` accepts a MIME wins. Then add the format to the corpus in
  `crates/quicksearch-core/tests/corpus/` and `tests/extraction_corpus.rs`;
  no corpus file may be written by the library that reads it back. New
  cascade behavior: `search/cascade.rs` documents the rank invariants
  that keep streamed results append-only.
- `packaging/capture.sh`: regenerates the website assets into
  `packaging/captures/` (gitignored) by building the GUI with the
  `capture` feature, whose scripted driver
  (`packaging/capture-scenario.txt`) records from the app's own
  framebuffer against a throwaway index under scratch XDG dirs. Needs a
  graphical session and ffmpeg with `libx264rgb` and `libvpx-vp9`.

To cut a release:

1. Bump `[workspace.package] version` in `Cargo.toml`.
2. Commit and push on a branch named `Release...` (CI runs
   `cargo update -w`, so the lockfile's own member versions follow).
3. Once both build jobs are green, CI tags the commit `v<version>` and
   publishes the release with the `.deb`, the AppImage and its `.zsync`
   sidecar, a Linux tarball, the Windows installer and a Windows zip.
   Pushing a `v*` tag by hand does the same thing.
4. Release builds run on the oldest supported LTS (an Ubuntu 22.04
   container): the builder's glibc becomes the `.deb`'s and the
   AppImage's floor.
