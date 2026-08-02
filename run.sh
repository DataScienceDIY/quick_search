#!/usr/bin/env sh
# Build and launch the QuickSearch GUI. On Unix the same binary doubles as the
# terminal search tool: ./target/release/quicksearch --help
# (On Windows that role belongs to quicksearch-cli.exe — see run.bat.)
set -e
cargo build --release -p quicksearch-gui
exec ./target/release/quicksearch "$@"
