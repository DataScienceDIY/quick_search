#!/usr/bin/env bash
#
# Build an AppImage for QuickSearch.
#
#   ./packaging/build-appimage.sh                 build and package
#   ./packaging/build-appimage.sh --no-build      package an existing release binary
#   ./packaging/build-appimage.sh --no-strip      keep debug symbols
#   ./packaging/build-appimage.sh -o /tmp/out     write the AppImage somewhere else
#
# Environment: SOURCE_DATE_EPOCH, APPIMAGETOOL.
#
# The AppDir bundles no libraries at all. That is not an oversight: the binary's
# dynamic section names only libgcc_s, libm and libc, because SQLCipher, OpenSSL
# and libdbus are linked statically, and winit and glutin dlopen the whole
# display stack at runtime. Those dlopened libraries - libGL, libEGL, libX11,
# libxcb, libXcursor, libXi, libXrender, libxkbcommon{,-x11} and
# libwayland-{client,egl} - are exactly the ones an AppImage must take from the
# host, since they have to match the user's graphics driver and compositor.
# Bundling them is how AppImages break on other people's machines.
#
# So the glibc floor is the only portability limit, and it is the same one the
# .deb carries: whatever the machine that built the binary provides. Release
# builds happen in a 22.04 container to keep that at 2.35.

set -euo pipefail
# Directories created along the way must be 0755, not whatever the caller's
# umask happens to be, matching build-deb.sh.
umask 022

readonly PKG=quicksearch
readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ICON_SRC="$REPO_ROOT/crates/quicksearch-gui/assets/icons"
readonly ICON_SVG="$ICON_SRC/quicksearch_icon.svg"
readonly METAINFO=com.karsttech.quicksearch.metainfo.xml

# Pinned rather than tracking the `continuous` tag, so a rebuild of an old
# commit uses the tool that commit was tested with. Bump the version and the
# checksum together; the checksum is the release asset's own digest.
readonly APPIMAGETOOL_VERSION=1.9.1
readonly APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
readonly APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-x86_64.AppImage"

# The runtime is the ~1 MB ELF stub prepended to the squashfs: it is the first
# thing that executes on a user's machine, so it matters more than the tool that
# assembles it. Left alone, appimagetool downloads it from the type2-runtime
# `continuous` tag mid-build, which would put an unpinned binary inside every
# release and make the checksum above mostly decorative. Pinned to a dated tag
# and passed in with --runtime-file instead.
readonly RUNTIME_VERSION=20251108
readonly RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
readonly RUNTIME_URL="https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-x86_64"

# Baked into every AppImage this produces, so AppImageUpdate can find newer
# builds. It cannot be corrected after the fact - a released binary keeps
# pointing wherever this said at the time - so moving the forge means the old
# URL has to keep resolving.
#
# Forgejo resolves the literal tag `latest` to the newest release and looks the
# asset up by name, so this path stays valid across releases. The GitHub-style
# /releases/latest/download/<asset> form is not implemented and 404s. The .zsync
# name therefore carries no version, while the AppImage it points at does; zsync
# resolves that from the sidecar's own headers, relative to this URL.
readonly UPDATE_URL="https://code.karsttech.com/jeremy/quick_search/releases/download/latest/$PKG-x86_64.AppImage.zsync"

do_build=1
do_strip=1
out_dir="$REPO_ROOT/dist"

die() { printf 'build-appimage: %s\n' "$*" >&2; exit 1; }
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

# zsyncmake comes from the zsync package. appimagetool looks it up in PATH
# rather than bundling it, and - worse - reports success and produces nothing
# when it is missing, so it is checked for here instead. appstreamcli comes from
# the appstream package. Unlike build-deb.sh, which is deliberately buildable
# with nothing but a stock Debian install, this script already downloads
# appimagetool, so a couple more packages cost nothing in reach.
for tool in curl sha256sum desktop-file-validate zsyncmake appstreamcli strings; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done
[ "$do_strip" -eq 0 ] || command -v strip >/dev/null 2>&1 || die "missing strip (install binutils, or pass --no-strip)"

# Version comes from [workspace.package] so the AppImage can never drift from
# the crate version.
version="$(sed -n '/^\[workspace\.package\]/,/^\[/{ s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p }' "$REPO_ROOT/Cargo.toml")"
[ -n "$version" ] || die "could not read version from Cargo.toml"

# Absolute from here on: appimagetool is run from a scratch directory below, and
# -o could well have been given a relative path.
mkdir -p "$out_dir"
out_dir="$(cd -- "$out_dir" && pwd)"

appdir="$out_dir/$PKG-$version-x86_64.AppDir"
appimage="$out_dir/$PKG-$version-x86_64.AppImage"
# Version-less on purpose: see UPDATE_URL above.
zsync="$out_dir/$PKG-x86_64.AppImage.zsync"

# Scratch space for appimagetool's stray output and the verification unpack.
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

# ------------------------------------------------------- appimagetool ------

cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/quicksearch"
appimagetool="${APPIMAGETOOL:-$cache_dir/appimagetool-$APPIMAGETOOL_VERSION-x86_64.AppImage}"
runtime="$cache_dir/runtime-$RUNTIME_VERSION-x86_64"

# Fetch to $1 from $2 if it is not cached, then check it against $3 on every
# run, not only after a download: a cached file is as much a supply-chain input
# as a freshly fetched one, and this is all that stands between a poisoned cache
# and a published release.
fetch_pinned() {
    local dest="$1" url="$2" sum="$3"
    if [ ! -f "$dest" ]; then
        say "Fetching $(basename -- "$dest")"
        mkdir -p "$(dirname -- "$dest")"
        curl --proto '=https' --tlsv1.2 -fsSL -o "$dest.part" "$url" \
            || die "could not download $url"
        mv -- "$dest.part" "$dest"
    fi
    printf '%s  %s\n' "$sum" "$dest" | sha256sum -c - >/dev/null 2>&1 \
        || die "checksum mismatch for $dest (delete it and retry)"
}

if [ -z "${APPIMAGETOOL:-}" ]; then
    fetch_pinned "$appimagetool" "$APPIMAGETOOL_URL" "$APPIMAGETOOL_SHA256"
    # curl leaves it 0644, and the cached copy is ours to chmod. A caller-supplied
    # one might be root-owned in /usr/bin, so that branch only checks.
    chmod +x "$appimagetool"
else
    [ -x "$appimagetool" ] || die "APPIMAGETOOL is not executable: $appimagetool"
fi

fetch_pinned "$runtime" "$RUNTIME_URL" "$RUNTIME_SHA256"

# ---------------------------------------------------------------- build ----

if [ "$do_build" -eq 1 ]; then
    say "Building quicksearch $version (release)"
    ( cd "$REPO_ROOT" && cargo build --release -p quicksearch-gui )
fi

[ -x "$REPO_ROOT/target/release/$PKG" ] \
    || die "no release binary at target/release/$PKG (drop --no-build?)"
[ -f "$ICON_SVG" ] || die "no icon at $ICON_SVG"
[ -f "$REPO_ROOT/packaging/$METAINFO" ] || die "no metainfo at packaging/$METAINFO"

say "Validating desktop entry"
desktop-file-validate "$REPO_ROOT/packaging/$PKG.desktop"

# --------------------------------------------------------------- stage -----

say "Staging $appdir"
rm -rf "$appdir"
mkdir -p "$appdir"

# Only the GUI binary ships, where build-deb.sh installs both: an AppImage has a
# single entry point, and on Unix `quicksearch` already does the terminal job
# too - quicksearch-cli is the same tool under a clearer name, which matters for
# something on PATH and not for a self-contained file the user runs directly.
install -Dm755 "$REPO_ROOT/target/release/$PKG"    "$appdir/usr/bin/$PKG"
install -Dm644 "$REPO_ROOT/packaging/$PKG.desktop" "$appdir/usr/share/applications/$PKG.desktop"
install -Dm644 "$ICON_SVG"                         "$appdir/usr/share/icons/hicolor/scalable/apps/$PKG.svg"
install -Dm644 "$REPO_ROOT/packaging/copyright"    "$appdir/usr/share/doc/$PKG/copyright"
install -Dm644 "$REPO_ROOT/config_example.toml"    "$appdir/usr/share/doc/$PKG/config_example.toml"
install -Dm644 "$REPO_ROOT/README.md"              "$appdir/usr/share/doc/$PKG/README.md"

# Same glob-driven discovery as build-deb.sh, including the guard: the glob
# would also catch a non-size name like quicksearch-cli.png.
shopt -s nullglob
icons=("$ICON_SRC"/$PKG-*.png)
shopt -u nullglob
[ "${#icons[@]}" -gt 0 ] || die "no icons in $ICON_SRC"
for png in "${icons[@]}"; do
    n="$(basename "$png" .png)"; n="${n#$PKG-}"
    case "$n" in ''|*[!0-9]*) die "unexpected icon name: $(basename "$png")" ;; esac
    install -Dm644 "$png" "$appdir/usr/share/icons/hicolor/${n}x${n}/apps/$PKG.png"
done
say "Installed ${#icons[@]} icon sizes plus the scalable SVG"

# @VERSION@/@DATE@ substitution, as build-deb.sh does for the man page .TH line,
# so [workspace.package] version stays the only place a release is bumped.
if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
    metainfo_date="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%d)"
else
    metainfo_date="$(date -u +%Y-%m-%d)"
fi
install -dm755 "$appdir/usr/share/metainfo"
sed -e "s/@VERSION@/$version/" -e "s/@DATE@/$metainfo_date/" \
    "$REPO_ROOT/packaging/$METAINFO" > "$appdir/usr/share/metainfo/$METAINFO"
chmod 644 "$appdir/usr/share/metainfo/$METAINFO"

# Validated here rather than in CI so a local build fails the same way, and on
# the substituted file rather than the @VERSION@ template. build-deb.sh ships
# the identical file, so this covers both packages. appimagetool would run its
# own check, but against whatever appstreamcli happens to be in PATH at the
# time; --no-appstream below turns that off in favour of this one.
say "Validating AppStream metainfo"
appstreamcli validate --no-net --explain "$appdir/usr/share/metainfo/$METAINFO"

# appimagetool looks for these three at the AppDir root. AppRun is a relative
# symlink rather than a wrapper script because nothing is bundled: there is no
# LD_LIBRARY_PATH or XDG_DATA_DIRS to set up, so a wrapper would only put a
# shell between the runtime and the app.
ln -s usr/bin/$PKG "$appdir/AppRun"
install -Dm644 "$ICON_SRC/$PKG-256.png" "$appdir/$PKG.png"
ln -s $PKG.png "$appdir/.DirIcon"
install -Dm644 "$REPO_ROOT/packaging/$PKG.desktop" "$appdir/$PKG.desktop"

if [ "$do_strip" -eq 1 ]; then
    before="$(du -h "$appdir/usr/bin/$PKG" | cut -f1)"
    strip --strip-unneeded "$appdir/usr/bin/$PKG"
    say "Stripped $PKG: $before -> $(du -h "$appdir/usr/bin/$PKG" | cut -f1)"
fi

# -------------------------------------------------------------- package ----

say "Building $appimage"
rm -f "$appimage" "$zsync"
# Run from the scratch directory: -u makes appimagetool shell out to zsyncmake,
# which writes its sidecar into the working directory rather than beside the
# output, and that stray would otherwise land in the repo root. The one this
# script actually ships is generated below.
#
# APPIMAGE_EXTRACT_AND_RUN makes appimagetool unpack itself instead of mounting
# itself, which is what lets this run in a container with no /dev/fuse. ARCH is
# set explicitly rather than left to autodetection.
( cd "$scratch" && ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 \
    "$appimagetool" --no-appstream --runtime-file "$runtime" \
        -u "zsync|$UPDATE_URL" "$appdir" "$appimage" )

[ -f "$appimage" ] || die "appimagetool produced no $appimage"
chmod 755 "$appimage"

# The shipped sidecar is generated here rather than left to appimagetool, which
# reports Success even when zsyncmake is absent and wrote nothing. Doing it
# directly also pins both headers: a bare relative name in URL: is what makes
# the stable, version-less sidecar name work, because zsync resolves it against
# wherever the sidecar was fetched from and so lands on the current release's
# versioned AppImage.
say "Generating $zsync"
zsyncmake -u "$(basename -- "$appimage")" -f "$(basename -- "$appimage")" \
    -o "$zsync" "$appimage"
[ -f "$zsync" ] || die "zsyncmake produced no $zsync"

rm -rf "$appdir"

# --------------------------------------------------------------- verify ----

say "Verifying $appimage"
( cd "$scratch" && "$appimage" --appimage-extract >/dev/null )

root="$scratch/squashfs-root"
for path in AppRun "$PKG.desktop" "$PKG.png" .DirIcon \
            "usr/bin/$PKG" "usr/share/applications/$PKG.desktop" \
            "usr/share/metainfo/$METAINFO"; do
    [ -e "$root/$path" ] || die "$path missing from the AppImage"
done
# The whole point of the layout: no bundled libraries.
[ ! -d "$root/usr/lib" ] || die "the AppDir grew a usr/lib - see the header comment"

# --version is handled before any window is created, so this works headless.
reported="$("$root/usr/bin/$PKG" --version)"
case "$reported" in
    *"v$version"*) ;;
    *) die "the packaged binary reports '$reported', expected v$version" ;;
esac

# A sidecar describing a different file is worse than no sidecar: AppImageUpdate
# would fetch and then reject every delta. Length is the cheap way to catch it.
zsync_len="$(sed -n 's/^Length: //p' "$zsync" | head -1)"
appimage_len="$(stat -c %s "$appimage")"
[ "$zsync_len" = "$appimage_len" ] \
    || die "$zsync describes $zsync_len bytes, but the AppImage is $appimage_len"

# The update information the runtime itself carries, which is what
# AppImageUpdate reads before it ever looks for a sidecar.
embedded="$(strings -a "$appimage" | grep -m1 '^zsync|' || true)"
[ "$embedded" = "zsync|$UPDATE_URL" ] \
    || die "embedded update info is '$embedded', expected 'zsync|$UPDATE_URL'"

echo
say "Done: $appimage"
echo "    reports:      $reported"
echo "    update info:  $UPDATE_URL"
echo "    sidecar:      $zsync"
echo "    run with:     chmod +x $appimage && $appimage"
