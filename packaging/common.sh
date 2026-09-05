# Shared preamble for the packaging scripts. Sourced, not executed:
#
#   . "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
#   parse_args "$@"
#
# Provides REPO_ROOT, the do_build/do_strip/out_dir defaults, die/say, the
# --no-build/--no-strip/-o/--help argument loop, and $version read from
# [workspace.package] — one copy, so the three scripts cannot drift apart.

readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

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

# Build the release binaries for the host target; both .deb and AppImage stage
# from target/release.
build_host_release() {
    say "Building quicksearch $version (release)"
    ( cd "$REPO_ROOT" && cargo build --release -p quicksearch-gui )
}
