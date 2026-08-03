#!/usr/bin/env bash
#
# Build a Debian package for QuickSearch.
#
#   ./packaging/build-deb.sh                 build and package
#   ./packaging/build-deb.sh --no-build      package an existing release binary
#   ./packaging/build-deb.sh --no-strip      keep debug symbols (25 MB vs 20 MB)
#   ./packaging/build-deb.sh -o /tmp/out     write the .deb somewhere else
#
# Environment: DEB_REVISION (default 1), DEB_MAINTAINER, SOURCE_DATE_EPOCH.
#
# Deliberately does not use cargo-deb, debhelper, fakeroot or an SVG rasteriser:
# dpkg-deb and desktop-file-utils are the only tools required, and both are part
# of a standard Debian or Ubuntu install.

set -euo pipefail
# Directories created along the way must be 0755, not whatever the caller's
# umask happens to be, or the package ships group-writable directories.
umask 022

readonly PKG=quicksearch
# The GUI binary and the console-subsystem terminal binary. Both ship: on Unix
# `quicksearch` does both jobs, but the README and the shared man page name
# `quicksearch-cli` too, so it has to exist wherever the docs are installed.
readonly BINARIES=(quicksearch quicksearch-cli)
readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ICON_SRC="$REPO_ROOT/crates/quicksearch-gui/assets/icons"
readonly ICON_SVG="$ICON_SRC/quicksearch_icon.svg"

do_build=1
do_strip=1
out_dir="$REPO_ROOT/dist"

die() { printf 'build-deb: %s\n' "$*" >&2; exit 1; }
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

for tool in dpkg-deb dpkg desktop-file-validate objdump gzip; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done
[ "$do_strip" -eq 0 ] || command -v strip >/dev/null 2>&1 || die "missing strip (install binutils, or pass --no-strip)"

# Version comes from [workspace.package] so the package can never drift from the
# crate version.
version="$(sed -n '/^\[workspace\.package\]/,/^\[/{ s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p }' "$REPO_ROOT/Cargo.toml")"
[ -n "$version" ] || die "could not read version from Cargo.toml"

revision="${DEB_REVISION:-1}"
maintainer="${DEB_MAINTAINER:-Jeremy <jeremy@karsttech.com>}"
arch="$(dpkg --print-architecture)"
deb_version="${version}-${revision}"
stage="$out_dir/${PKG}_${deb_version}_${arch}"
deb="$out_dir/${PKG}_${deb_version}_${arch}.deb"

# ---------------------------------------------------------------- build ----

if [ "$do_build" -eq 1 ]; then
    say "Building quicksearch $version (release)"
    ( cd "$REPO_ROOT" && cargo build --release -p quicksearch-gui )
fi

for bin in "${BINARIES[@]}"; do
    [ -x "$REPO_ROOT/target/release/$bin" ] \
        || die "no release binary at target/release/$bin (drop --no-build?)"
done
# Both binaries link the same crates, so either gives the same glibc floor.
readonly primary_binary="$REPO_ROOT/target/release/$PKG"
[ -f "$ICON_SVG" ] || die "no icon at $ICON_SVG"

say "Validating desktop entry"
desktop-file-validate "$REPO_ROOT/packaging/$PKG.desktop"

# --------------------------------------------------------------- stage -----

say "Staging $stage"
rm -rf "$stage"
mkdir -p "$stage"

for bin in "${BINARIES[@]}"; do
    install -Dm755 "$REPO_ROOT/target/release/$bin"     "$stage/usr/bin/$bin"
done
install -Dm644 "$REPO_ROOT/packaging/$PKG.desktop"      "$stage/usr/share/applications/$PKG.desktop"
install -Dm644 "$ICON_SVG"                              "$stage/usr/share/icons/hicolor/scalable/apps/$PKG.svg"
install -Dm644 "$REPO_ROOT/packaging/copyright"         "$stage/usr/share/doc/$PKG/copyright"
install -Dm644 "$REPO_ROOT/config_example.toml"         "$stage/usr/share/doc/$PKG/config_example.toml"
install -Dm644 "$REPO_ROOT/README.md"                   "$stage/usr/share/doc/$PKG/README.md"

# The PNGs are committed next to the quicksearch_icon.svg they came from, so
# this script only copies them and an ordinary `cargo build` needs no image
# tooling. To re-render them after editing the SVG, build a throwaway crate
# (outside this workspace, to keep it out of Cargo.lock) depending on
# `resvg = { version = "0.45", default-features = false }` and, for each size N,
# parse with usvg::Tree::from_data, make a tiny_skia::Pixmap::new(N, N), call
# resvg::render with Transform::from_scale(N/240.0, N/240.0) and save_png. The
# SVG contains no <text>, so no font support is needed.
#
# Sizes follow whatever is committed, so adding one needs no script change.
shopt -s nullglob
icons=("$ICON_SRC"/$PKG-*.png)
shopt -u nullglob
[ "${#icons[@]}" -gt 0 ] || die "no icons in $ICON_SRC"
for png in "${icons[@]}"; do
    n="$(basename "$png" .png)"; n="${n#$PKG-}"
    # The glob would also catch a non-size name like quicksearch-cli.png.
    case "$n" in ''|*[!0-9]*) die "unexpected icon name: $(basename "$png")" ;; esac
    install -Dm644 "$png" "$stage/usr/share/icons/hicolor/${n}x${n}/apps/$PKG.png"
done
say "Installed ${#icons[@]} icon sizes plus the scalable SVG"

# Debian wants man pages and the changelog compressed, with no gzip timestamp so
# repeat builds are byte-identical. quicksearch-cli.1 is a one-line .so stub
# pointing at quicksearch.1, which documents both binaries.
install -dm755 "$stage/usr/share/man/man1"
for page in "${BINARIES[@]}"; do
    gzip -9nc "$REPO_ROOT/packaging/$page.1" > "$stage/usr/share/man/man1/$page.1.gz"
    chmod 644 "$stage/usr/share/man/man1/$page.1.gz"
done

if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
    changelog_date="$(date -R -u -d "@$SOURCE_DATE_EPOCH")"
else
    changelog_date="$(date -R)"
fi
gzip -9nc <<EOF > "$stage/usr/share/doc/$PKG/changelog.Debian.gz"
$PKG ($deb_version) unstable; urgency=medium

  * Package build of $PKG $version.

 -- $maintainer  $changelog_date
EOF
chmod 644 "$stage/usr/share/doc/$PKG/changelog.Debian.gz"

# No config.toml is installed anywhere. Config::config_path() treats a
# config.toml sitting next to the executable as portable mode and lets it
# override the per-user config outright, so one in /usr/bin would hijack every
# account on the machine. Config::load_from creates ~/.config/quicksearch/
# config.toml on first run instead.

if [ "$do_strip" -eq 1 ]; then
    for bin in "${BINARIES[@]}"; do
        before="$(du -h "$stage/usr/bin/$bin" | cut -f1)"
        strip --strip-unneeded "$stage/usr/bin/$bin"
        say "Stripped $bin: $before -> $(du -h "$stage/usr/bin/$bin" | cut -f1)"
    done
fi

installed_size="$(du -ks "$stage" | cut -f1)"

# ------------------------------------------------------------- control -----

# The dynamic section only names libc, libgcc, libm and libbz2: winit and glutin
# dlopen the entire display stack, so dpkg-shlibdeps cannot see it and the list
# below is maintained by hand. Re-derive it with
#   objdump -p target/release/quicksearch | grep NEEDED
#   strings -a target/release/quicksearch | grep -oE 'lib[A-Za-z0-9_+-]+\.so(\.[0-9]+)*' | sort -u
# and map each soname to a package with `dpkg -S`.
glibc_min="$(objdump -T "$primary_binary" | sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' | sort -V | tail -1)"
[ -n "$glibc_min" ] || die "could not determine the glibc version requirement"

depends="libc6 (>= ${glibc_min}), libgcc-s1 (>= 3.0), libbz2-1.0"
depends="$depends, libx11-6, libxcb1, libxcursor1, libxi6, libxrender1"
depends="$depends, libxkbcommon0, libxkbcommon-x11-0"
depends="$depends, libwayland-client0, libwayland-egl1"
depends="$depends, libegl1, libgl1"
# Required by policy for anything installing into the hicolor theme; it also
# provides the dpkg trigger that refreshes the icon cache on install.
depends="$depends, hicolor-icon-theme"

# desktop-file-utils owns the /usr/share/applications trigger; dbus-bin provides
# the dbus-send used by "reveal in folder"; xdg-utils provides the xdg-open
# fallback; xdg-desktop-portal backs the native folder picker. None are needed
# to search, so none are hard dependencies.
#
# The index-password "remember on this device" feature talks Secret Service
# over the session bus with a statically linked libdbus (keyring's `vendored`
# feature), so it adds no library dependency; without a Secret Service
# provider (gnome-keyring, kwalletd) it simply falls back to prompting.
recommends="desktop-file-utils, xdg-utils, dbus-bin, xdg-desktop-portal"

install -dm755 "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<EOF
Package: $PKG
Version: $deb_version
Section: utils
Priority: optional
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $installed_size
Depends: $depends
Recommends: $recommends
Homepage: https://code.karsttech.com/jeremy/quick_search
Description: fast full-text search across your files
 QuickSearch keeps a SQLite/FTS5 index of the directories you choose and
 searches them by both filename and file content. It extracts text from
 documents, PDFs, archives and office files, watches the indexed paths for
 changes and reindexes in the background while the application is open.
 .
 The same binary doubles as a terminal search tool: "quicksearch <terms>"
 prints ranked results and exits without starting the indexer, the file
 watcher or any background thread.
EOF

# No postinst/postrm: hicolor-icon-theme and desktop-file-utils declare
# interest-noawait on /usr/share/icons/hicolor and /usr/share/applications, so
# dpkg refreshes both caches on install and removal by itself.

# --------------------------------------------------------------- build -----

say "Building $deb"
dpkg-deb --root-owner-group --build "$stage" "$deb" >/dev/null
rm -rf "$stage"

echo
dpkg-deb --info "$deb"
echo
say "Done: $deb"
echo "    install with:  sudo apt install $deb"
echo "    inspect with:  dpkg-deb --contents $deb"
