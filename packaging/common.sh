# Shared preamble for the packaging scripts. Sourced, not executed:
#
#   . "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
#   parse_args "$@"
#
# Provides REPO_ROOT, the do_build/do_strip/out_dir defaults, die/say, the
# --no-build/--no-strip/-o/--help argument loop, $version read from
# [workspace.package], and the staging steps the .deb and the AppImage share:
# one copy, so the scripts cannot drift apart. The staging steps read the
# caller's $PKG.

# Directories created along the way must be 0755, not whatever the caller's
# umask happens to be, or a package ships group-writable directories.
umask 022

readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ICON_SRC="$REPO_ROOT/crates/quicksearch-gui/assets/icons"
readonly ICON_SVG="$ICON_SRC/quicksearch_icon.svg"
readonly METAINFO=com.karsttech.quicksearch.metainfo.xml

do_build=1
do_strip=1
out_dir="$REPO_ROOT/dist"

die() { printf '%s: %s\n' "$(basename -- "$0" .sh)" "$*" >&2; exit 1; }
say() { printf '\033[1m==>\033[0m %s\n' "$*"; }

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --no-build) do_build=0 ;;
            --no-strip) do_strip=0 ;;
            -o|--output-dir) shift; [ $# -gt 0 ] || die "--output-dir needs a path"; out_dir="$1" ;;
            # Print the calling script's header comment block, however long
            # it grows. `$0` is the caller even from a sourced file.
            -h|--help) awk 'NR > 1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"; exit 0 ;;
            *) die "unknown option: $1 (try --help)" ;;
        esac
        shift
    done
}

# Version comes from [workspace.package] so no artifact can drift from the
# crate version.
version="$(sed -n '/^\[workspace\.package\]/,/^\[/{ s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p }' "$REPO_ROOT/Cargo.toml")"
[ -n "$version" ] || die "could not read version from Cargo.toml"

# Die unless every named tool is on PATH, and strip too unless --no-strip.
# Call after parse_args.
require_tools() {
    local tool
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
    done
    [ "$do_strip" -eq 0 ] || command -v strip >/dev/null 2>&1 || die "missing strip (install binutils, or pass --no-strip)"
}

# Build the release binaries for the host target; both .deb and AppImage stage
# from target/release.
build_host_release() {
    say "Building quicksearch $version (release)"
    ( cd "$REPO_ROOT" && cargo build --release -p quicksearch-gui )
}

# The copyright file and the documents both Linux packages carry, under $1.
install_docs() {
    local doc="$1/usr/share/doc/$PKG"
    install -Dm644 "$REPO_ROOT/packaging/copyright" "$doc/copyright"
    install -Dm644 "$REPO_ROOT/config_example.toml" "$doc/config_example.toml"
    install -Dm644 "$REPO_ROOT/README.md"           "$doc/README.md"
}

# The scalable icon and every committed PNG size, into $1's hicolor theme.
#
# The PNGs are committed next to the quicksearch_icon.svg they came from, so
# packaging only copies them and an ordinary `cargo build` needs no image
# tooling. To re-render them after editing the SVG, build a throwaway crate
# (outside this workspace, to keep it out of Cargo.lock) depending on
# `resvg = { version = "0.45", default-features = false }` and, for each size N,
# parse with usvg::Tree::from_data, make a tiny_skia::Pixmap::new(N, N), call
# resvg::render with Transform::from_scale(N/240.0, N/240.0) and save_png. The
# SVG contains no <text>, so no font support is needed.
#
# Sizes follow whatever is committed, so adding one needs no change here.
install_icons() {
    local icons=() png n
    install -Dm644 "$ICON_SVG" "$1/usr/share/icons/hicolor/scalable/apps/$PKG.svg"
    shopt -s nullglob
    icons=("$ICON_SRC"/$PKG-*.png)
    shopt -u nullglob
    [ "${#icons[@]}" -gt 0 ] || die "no icons in $ICON_SRC"
    for png in "${icons[@]}"; do
        n="$(basename "$png" .png)"; n="${n#$PKG-}"
        # The glob would also catch a non-size name like quicksearch-cli.png.
        case "$n" in ''|*[!0-9]*) die "unexpected icon name: $(basename "$png")" ;; esac
        install -Dm644 "$png" "$1/usr/share/icons/hicolor/${n}x${n}/apps/$PKG.png"
    done
    say "Installed ${#icons[@]} icon sizes plus the scalable SVG"
}

# The AppStream metadata, into $1, with @VERSION@ and @DATE@ filled in the way
# build-deb.sh fills the man page's .TH line, so [workspace.package] version
# stays the only thing a release bumps. SOURCE_DATE_EPOCH pins the date.
install_metainfo() {
    local dir="$1/usr/share/metainfo" day
    day="$(date -u ${SOURCE_DATE_EPOCH:+-d "@$SOURCE_DATE_EPOCH"} +%F)"
    install -dm755 "$dir"
    sed -e "s/@VERSION@/$version/" -e "s/@DATE@/$day/" \
        "$REPO_ROOT/packaging/$METAINFO" > "$dir/$METAINFO"
    chmod 644 "$dir/$METAINFO"
}
