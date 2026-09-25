#!/usr/bin/env bash
#
# Build a Debian package for QuickSearch.
#
#   ./packaging/build-deb.sh                 build and package
#   ./packaging/build-deb.sh --no-build      package an existing release binary
#   ./packaging/build-deb.sh --no-strip      keep debug symbols (25 MB vs 20 MB)
#   ./packaging/build-deb.sh -o /tmp/out     write the .deb somewhere else
#
# Environment: DEB_MAINTAINER, SOURCE_DATE_EPOCH.
#
# Deliberately does not use cargo-deb, debhelper, fakeroot or an SVG rasteriser:
# dpkg-deb and desktop-file-utils are the only tools required, and both are part
# of a standard Debian or Ubuntu install.

set -euo pipefail

readonly PKG=quicksearch
# The GUI binary and the console-subsystem terminal binary. Both ship: on Unix
# `quicksearch` does both jobs, but the README and the shared man page name
# `quicksearch-cli` too, so it has to exist wherever the docs are installed.
readonly BINARIES=(quicksearch quicksearch-cli)
. "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
parse_args "$@"

require_tools dpkg-deb dpkg desktop-file-validate objdump gzip

# No Debian revision: QuickSearch is only ever packaged from its own source, so
# the version is native (no hyphen) and the .deb is named for the crate version
# alone, matching the Windows installer.
maintainer="${DEB_MAINTAINER:-Jeremy <jeremy@karsttech.com>}"
arch="$(dpkg --print-architecture)"
stage="$out_dir/${PKG}_${version}_${arch}"
deb="$out_dir/${PKG}_${version}_${arch}.deb"

# ---------------------------------------------------------------- build ----

if [ "$do_build" -eq 1 ]; then
    build_host_release
fi

for bin in "${BINARIES[@]}"; do
    [ -x "$REPO_ROOT/target/release/$bin" ] \
        || die "no release binary at target/release/$bin (drop --no-build?)"
done
# Both binaries link the same crates, so either gives the same glibc floor.
readonly primary_binary="$REPO_ROOT/target/release/$PKG"
[ -f "$ICON_SVG" ] || die "no icon at $ICON_SVG"
[ -f "$REPO_ROOT/packaging/$METAINFO" ] || die "no metainfo at packaging/$METAINFO"

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
install_docs "$stage"
install_icons "$stage"

# AppStream metadata, so software centres show a real listing rather than a bare
# package name. build-appimage.sh ships the identical file and is where it gets
# validated - appstreamcli is not something a stock Debian install has, and this
# script deliberately needs nothing beyond dpkg-deb and desktop-file-utils.
install_metainfo "$stage"

# Debian wants man pages and the changelog compressed, with no gzip timestamp so
# repeat builds are byte-identical. quicksearch-cli.1 is a one-line .so stub
# pointing at quicksearch.1, which documents both binaries.
#
# The .TH version field is rewritten from $version on the way through, so the
# footer of an installed page tracks [workspace.package] version instead of
# needing a hand-edit every release. The stub has no .TH, so the sed is a
# no-op there.
install -dm755 "$stage/usr/share/man/man1"
for page in "${BINARIES[@]}"; do
    sed "/^\.TH /s/\"quicksearch [^\"]*\"/\"quicksearch $version\"/" \
        "$REPO_ROOT/packaging/$page.1" \
        | gzip -9nc > "$stage/usr/share/man/man1/$page.1.gz"
    chmod 644 "$stage/usr/share/man/man1/$page.1.gz"
done

changelog_date="$(date -R ${SOURCE_DATE_EPOCH:+-u -d "@$SOURCE_DATE_EPOCH"})"
# changelog.gz, not changelog.Debian.gz: with a native version there is no
# separate Debian revision to log, and policy puts the one changelog here.
gzip -9nc <<EOF > "$stage/usr/share/doc/$PKG/changelog.gz"
$PKG ($version) unstable; urgency=medium

  * Package build of $PKG $version.

 -- $maintainer  $changelog_date
EOF
chmod 644 "$stage/usr/share/doc/$PKG/changelog.gz"

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
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $installed_size
Depends: $depends
Recommends: $recommends
Homepage: https://code.karsttech.com/jeremy/quick_search
Description: fast full-text search across your files
 QuickSearch indexes the folders you choose and searches them by file name
 and content. It reads the text of plain-text files, PDFs, RTF and office
 documents, and the tags of audio files, and keeps the index current while
 the app is open.
 .
 It also searches from a terminal: "quicksearch" followed by search terms
 prints ranked results without starting the indexer.
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
