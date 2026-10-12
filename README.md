# QuickSearch

A fast local file search tool. QuickSearch indexes the folders you choose and
keeps that index fresh automatically, so it finds files by name or by content
as fast as you type.

## AI Disclaimer

QuickSearch's core was designed and built by hand by its developer. Most of
the rest, including the GUI, was designed by a human and built with AI agents,
under human review, improvement and testing.

## Home & GitHub Mirror

The primary home of this software is:

- https://quicksearch.karsttech.com
- https://code.karsttech.com/jeremy/quick_search

The code is also mirrored to GitHub for easier bug reporting and issue
tracking: https://github.com/DataScienceDIY/quick_search

## Features

- **Search as you type**: ranked results update with every keystroke,
  matching file names, folder paths and file contents.
- **Typo tolerance**: an optional fuzzy mode finds `repot` when you meant
  `report`.
- **Query filters**: narrow a search by kind, date or location, e.g.
  `budget type:Document modified:>=2024-01-01`, plus wildcards and regular
  expressions. The in-app "?" window lists the syntax.
- **Live results**: results on screen pick up renames, edits and deletions,
  even while indexing is paused.
- **Duplicate finder**: groups files that look identical and verifies a group
  byte for byte on request. It never deletes.
- **Always fresh**: folders are watched and periodically reindexed, or, in
  manual mode, indexed only when you say so.
- **Password protection**: optionally encrypt the index, which holds the
  names and text of everything indexed; your system keychain can remember
  the key.
- **Global shortcut**: Ctrl+Shift+F brings up the window from anywhere,
  starting QuickSearch if it is closed. The Settings tab binds it in one
  click on KDE and GNOME; elsewhere, bind `quicksearch --toggle` in your
  desktop's keyboard settings.
- **Terminal search**: `quicksearch <query>` prints ranked, pipe-friendly
  paths; `--long` adds sizes, dates and highlighted snippets.
- **Portable mode**: keep the program, config and index in one folder that
  moves between devices.

The Help tab and first-start tour cover usage, and every setting explains
itself on hover. `man quicksearch` (from the .deb, or
`packaging/quicksearch.1`) documents terminal search and the query syntax.

## Build & run

```sh
./build.sh          # Linux
build.bat           # Windows
```

These take a fresh machine to a running app: they install missing build
dependencies (via sudo or winget, and Rust via rustup), build a release binary,
and launch it. `--check` lists what is missing and installs nothing.

```sh
cargo build --release -p quicksearch-gui   # binaries: target/release/quicksearch{,-cli}
cargo run -p quicksearch-gui               # or just run it
cargo test --release --workspace           # test suite (release: debug Argon2 takes minutes)
```

## Install

- **Debian / Ubuntu**: `./packaging/build-deb.sh`, then
  `sudo apt install ./dist/quicksearch_<version>_amd64.deb`.
- **Other Linux**: download the AppImage from the release page, make it
  executable, and run it. If your distribution lacks FUSE, run it with
  `APPIMAGE_EXTRACT_AND_RUN=1`.
- **Windows**: run the setup `.exe` from the release page, or unpack the
  `.zip` anywhere for an install-free copy.

## Configuration

Settings live in `~/.config/quicksearch/config.toml` (Windows:
`%APPDATA%\quicksearch\config.toml`), created on first run and edited live
by the GUI. [config_example.toml](config_example.toml) is the full
reference for every key. A QuickSearch `config.toml` placed next to the
binary (or the `.AppImage`) enables portable mode; another program's file of
that name is ignored.

## More

Design choices and the reasoning behind them are described in
[DESIGN.md](DESIGN.md); the code and its comments are the detailed
reference. QuickSearch is open source under the terms in
[LICENSE](LICENSE).
