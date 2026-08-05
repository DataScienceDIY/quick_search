#!/usr/bin/env bash
#
# Build the Windows installer for QuickSearch.
#
#   ./packaging/build-installer.sh                 build and package
#   ./packaging/build-installer.sh --no-build      package existing binaries
#   ./packaging/build-installer.sh --no-strip      keep debug symbols
#   ./packaging/build-installer.sh -o /tmp/out     write the installer elsewhere
#
# Runs on Linux and produces dist/quicksearch-<version>-windows-x86_64-setup.exe.
# The binaries come from the x86_64-pc-windows-gnu target and makensis compiles
# the installer, so no Windows machine is involved - CI builds this in the same
# job that produces the .zip.
#
# Needs: makensis (the nsis package) and a mingw-w64 toolchain
# (gcc-mingw-w64-x86-64, which brings x86_64-w64-mingw32-strip).
#
# The installer itself is packaging/quicksearch.nsi; this script only decides
# what goes into it.

set -euo pipefail
umask 022

readonly PKG=quicksearch
readonly TARGET=x86_64-pc-windows-gnu
# The GUI app and the console-subsystem terminal binary. Windows needs both as
# separate executables - see the [[bin]] comment in crates/quicksearch-gui.
readonly BINARIES=(quicksearch.exe quicksearch-cli.exe)
readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ICON="$REPO_ROOT/crates/quicksearch-gui/assets/icons/$PKG.ico"

do_build=1
do_strip=1
out_dir="$REPO_ROOT/dist"

die() { printf 'build-installer: %s\n' "$*" >&2; exit 1; }
say() { printf '\033[1m==>\033[0m %s\n' "$*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) do_build=0 ;;
        --no-strip) do_strip=0 ;;
        -o|--output-dir) shift; [ $# -gt 0 ] || die "--output-dir needs a path"; out_dir="$1" ;;
        # Print the header comment block, however long it grows.
        -h|--help) awk 'NR > 1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
done

command -v makensis >/dev/null 2>&1 || die "missing makensis (install nsis)"
[ "$do_strip" -eq 0 ] || command -v x86_64-w64-mingw32-strip >/dev/null 2>&1 \
    || die "missing x86_64-w64-mingw32-strip (install gcc-mingw-w64-x86-64, or pass --no-strip)"

# Same source of truth as build-deb.sh and the CI asset names: the crate version,
# never a tag or a branch name.
version="$(sed -n '/^\[workspace\.package\]/,/^\[/{ s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p }' "$REPO_ROOT/Cargo.toml")"
[ -n "$version" ] || die "could not read version from Cargo.toml"

# VIProductVersion accepts nothing but four numeric fields, so 0.9.1 has to
# become 0.9.1.0 and a pre-release suffix has to come off. The visible version
# string keeps whatever Cargo.toml says.
version_quad="${version%%[-+]*}"
case "$version_quad" in
    *[!0-9.]*) die "cannot build a Windows version number from '$version'" ;;
esac
while [ "$(printf '%s' "$version_quad" | tr -cd . | wc -c)" -lt 3 ]; do
    version_quad="$version_quad.0"
done

installer="$out_dir/$PKG-$version-windows-x86_64-setup.exe"
stage="$out_dir/.installer-stage"

# ---------------------------------------------------------------- build ----

if [ "$do_build" -eq 1 ]; then
    say "Building $PKG $version for $TARGET (release)"
    # rustc and the cc crate both derive these from the triple on most setups,
    # but rusqlite, zstd-sys and openssl-src shell out to a C compiler and a
    # host cc would produce ELF objects the mingw linker then rejects with an
    # error that names neither. Setting them costs nothing and CI's identical
    # values win by ':=' anyway.
    : "${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER:=x86_64-w64-mingw32-gcc}"
    : "${CC_x86_64_pc_windows_gnu:=x86_64-w64-mingw32-gcc}"
    : "${AR_x86_64_pc_windows_gnu:=x86_64-w64-mingw32-ar}"
    export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER CC_x86_64_pc_windows_gnu AR_x86_64_pc_windows_gnu
    ( cd "$REPO_ROOT" && cargo build --release -p quicksearch-gui --target "$TARGET" )
fi

for bin in "${BINARIES[@]}"; do
    [ -f "$REPO_ROOT/target/$TARGET/release/$bin" ] \
        || die "no binary at target/$TARGET/release/$bin (drop --no-build?)"
done
[ -f "$ICON" ] || die "no icon at $ICON"

# --------------------------------------------------------------- stage -----

say "Staging $stage"
rm -rf "$stage"
mkdir -p "$stage"

for bin in "${BINARIES[@]}"; do
    install -m755 "$REPO_ROOT/target/$TARGET/release/$bin" "$stage/$bin"
    if [ "$do_strip" -eq 1 ]; then
        before="$(du -h "$stage/$bin" | cut -f1)"
        x86_64-w64-mingw32-strip --strip-unneeded "$stage/$bin"
        say "Stripped $bin: $before -> $(du -h "$stage/$bin" | cut -f1)"
    fi
done

# The icon is the installer's own icon, the uninstaller's, the Start menu
# shortcut's, and what Add/Remove Programs shows. The .exe files carry no icon
# resource of their own, so it has to be installed as a file.
install -m644 "$ICON" "$stage/$PKG.ico"

# CRLF for the text files. The license page is a Windows edit control fed the
# file verbatim, and LF-only text arrives there as one long paragraph; the same
# conversion is what makes the installed copies readable in Notepad. LICENSE
# also gains the extension Windows needs to open it on a double-click.
for doc in README.md:README.md LICENSE:LICENSE.txt config_example.toml:config_example.toml; do
    sed 's/$/\r/' "$REPO_ROOT/${doc%%:*}" > "$stage/${doc##*:}"
    chmod 644 "$stage/${doc##*:}"
done

# ----------------------------------------------------------- makensis -----

say "Building $installer"
rm -f "$installer"
makensis -V3 \
    "-DVERSION=$version" \
    "-DVERSION_QUAD=$version_quad" \
    "-DSTAGE=$stage" \
    "-DOUTFILE=$installer" \
    "$REPO_ROOT/packaging/$PKG.nsi"

rm -rf "$stage"
[ -f "$installer" ] || die "makensis reported success but wrote no installer"

echo
say "Done: $installer ($(du -h "$installer" | cut -f1))"
