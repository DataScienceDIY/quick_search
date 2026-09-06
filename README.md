# QuickSearch

A fast local file search tool. QuickSearch indexes the folders you choose,
keeps that index fresh automatically. It finds files by name or by text
content as fast as you type.

## AI Disclaimer

QuickSearch has a core designed by its developer and built by hand, however
the majority of its codebase including its GUI was designed by a human and
built using AI agents with human review, improvements, and testing.

## Home & GitHub Mirror

The primary home of this software is:

- https://quicksearch.karsttech.com
- https://code.karsttech.com/jeremy/quick_search

The code is also mirrored to GitHub for easier bug reporting and issue
tracking: https://github.com/DataScienceDIY/quick_search

## Features

- **Search as you type** — ranked results appear instantly and update with
  every keystroke, matching file names, folder paths, and file contents.
- **Typo tolerance** — an optional fuzzy mode finds `repot` when you meant
  `report`.
- **Query filters** — narrow a search by kind, date, or location, e.g.
  `budget type:Document modified:>=2024-01-01`, with wildcards and regular
  expressions for power users. The in-app "?" popup documents the syntax.
- **Live results** — results on screen notice renames, edits, and deletions
  within a second, even while indexing is paused.
- **Duplicate finder** — groups files that appear identical, and can verify
  any group byte for byte on request. It only reports; it never deletes.
- **Always fresh** — folders are watched for changes and reindexed
  periodically, or switch to manual mode and index only when you say so.
- **Password protection** — optionally encrypt the index, since it contains
  the names and text of everything indexed. The password can be remembered
  in your system keychain.
- **Global shortcut** — Ctrl+Shift+F brings up the window from anywhere,
  starting QuickSearch if it is closed. One click on the Settings tab binds
  it on KDE and GNOME; elsewhere, bind `quicksearch --toggle` in your
  desktop's keyboard settings.
- **Terminal search** — `quicksearch <query>` prints ranked, pipe-friendly
  paths; `--long` adds sizes, dates, and highlighted snippets.
- **Portable mode** — keep the program, its config, and its index together
  in one folder that can be moved between devices.

The in-app Help tab and first-start tour cover usage; every setting is
explained on hover in the Settings tab.

## Build & run

```sh
./build.sh          # Linux
build.bat           # Windows
```

These take a fresh machine to a running app: they install missing build
dependencies (with your permission), build a release binary, and launch it.

```sh
cargo build --release -p quicksearch-gui   # binaries: target/release/quicksearch{,-cli}
cargo run -p quicksearch-gui               # or just run it
cargo test -p quicksearch-core             # test suite
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
reference for every key. A `config.toml` placed next to the binary enables
portable mode.

## More

Design choices and the reasoning behind them are described in
[DESIGN.md](DESIGN.md); the code and its comments are the detailed
reference. QuickSearch is open source under the terms in
[LICENSE](LICENSE).
