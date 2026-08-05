#!/usr/bin/env sh
#
# Set up the build environment, build QuickSearch, and launch the GUI. On Unix
# the same binary doubles as the terminal search tool:
#     ./target/release/quicksearch --help
# (On Windows that role belongs to quicksearch-cli.exe — see build.bat.)
#
#   ./build.sh                 install what is missing, build release, launch
#   ./build.sh --no-run        stop after the build
#   ./build.sh --check         report dependency status; install and build nothing
#   ./build.sh --installer     build the Windows installer instead of running
#   ./build.sh -- <args>       pass everything after -- to the launched binary
#
# Anything the script does not recognise ends its own option parsing, so a bare
# search term reaches the binary; use -- for arguments that look like flags.
#
# On a fresh Linux machine this installs the C toolchain, Perl and pkg-config
# with the distribution's package manager (via sudo), then the Rust toolchain
# with rustup. Every stage is skipped when what it provides is already present,
# so the everyday run costs one `cargo build`.
# Other Unixes only get the Rust stage — see the README for their toolchains.
#
# --installer adds NSIS and the mingw-w64 cross toolchain to that list and hands
# off to packaging/build-installer.sh, which cross-compiles and produces
# dist/quicksearch-<version>-windows-x86_64-setup.exe. Arguments after -- go to
# that script, so `./build.sh --installer -- --no-strip` works. It is opt-in
# because neither tool has anything to do with building or running QuickSearch
# here: an ordinary ./build.sh should not install a Windows installer compiler.
set -e

# Not `dirname`: a script whose job is to install missing tools should lean on
# as few of them as possible. $0 has no slash when the script is found on PATH.
case "$0" in
    */*) script_dir="${0%/*}" ;;
    *)   script_dir=. ;;
esac
REPO_ROOT="$(cd -- "$script_dir" && pwd)"

die() { printf 'build: %s\n' "$*" >&2; exit 1; }
say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
need_cmd() { command -v "$1" >/dev/null 2>&1; }

do_run=1
mode=run
want_installer=0

while [ $# -gt 0 ]; do
    case "$1" in
        --no-run) do_run=0 ;;
        --check) mode=check ;;
        # There is no Linux binary to launch at the end of an installer build.
        --installer) want_installer=1; do_run=0 ;;
        # Print the header comment block, however long it grows.
        -h|--help) awk 'NR > 1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"; exit 0 ;;
        --) shift; break ;;
        # Not ours: it and everything after it belongs to the binary.
        *) break ;;
    esac
    shift
done

# ------------------------------------------------------------ toolchain ----

# cargo may be installed but absent from this shell's PATH, which is the normal
# state right after rustup runs: it appends to the shell profile, and
# ~/.cargo/env is what it writes for processes that cannot wait for a new login.
have_cargo() {
    if need_cmd cargo; then return 0; fi
    if [ -r "$HOME/.cargo/env" ]; then
        . "$HOME/.cargo/env"
        if need_cmd cargo; then return 0; fi
    fi
    return 1
}

ensure_rust() {
    if have_cargo; then return 0; fi
    need_cmd curl || die "curl is needed to fetch rustup"
    say "Installing the Rust toolchain (rustup, into ~/.rustup and ~/.cargo)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable
    if [ -r "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
    need_cmd cargo || die "rustup finished but cargo is still not on PATH"
}

# -------------------------------------------------------- system packages ---

# Sets $missing to the space-separated tools that are absent. Probing for the
# command rather than for a package keeps this identical across distributions
# and honours toolchains installed outside the package manager.
#
# Perl is not optional despite never being named in the source: rusqlite's
# bundled-sqlcipher-vendored-openssl feature builds OpenSSL, whose Configure is
# a Perl script. Without it the failure comes hundreds of crates into the build.
check_deps() {
    missing=''
    if ! need_cmd cc && ! need_cmd gcc && ! need_cmd clang; then missing="$missing cc"; fi
    need_cmd make || missing="$missing make"
    need_cmd perl || missing="$missing perl"
    # pkg-config is how x11-dl and wayland-sys locate the display libraries.
    # No -dev packages for those libraries are needed: winit dlopens the whole
    # display stack at run time (xkbcommon-dl has no build script at all, and
    # x11-dl's records a libdir of None when a .pc file is absent), so headers
    # never enter the build.
    need_cmd pkg-config || missing="$missing pkg-config"
    # curl is a build dependency only while rustup still has to be downloaded.
    if ! have_cargo && ! need_cmd curl; then missing="$missing curl"; fi
    # Only for --installer: makensis compiles the .nsi into the installer, and
    # the mingw-w64 gcc is the linker and the C compiler for the bundled
    # SQLCipher, OpenSSL and zstd sources on the Windows target. It also brings
    # in the cross binutils, which is where the windres the GUI's build script
    # uses to compile the .exe version resource comes from. The Rust side
    # needs nothing extra — rust-toolchain.toml already lists the target, so
    # rustup installs it with the pinned toolchain.
    if [ "$want_installer" = 1 ]; then
        need_cmd makensis || missing="$missing makensis"
        need_cmd x86_64-w64-mingw32-gcc || missing="$missing x86_64-w64-mingw32-gcc"
    fi
}

# The tool -> package mapping, per package manager. Packages named twice (a
# single build-essential covers both cc and make) are deduplicated by the caller.
# Printing nothing means "this distribution has no package for it in its default
# repositories", which the caller reports rather than guessing a name; the two
# --installer tools are the only ones where that happens.
packages_for() {
    case "$1:$2" in
        apt-get:cc|apt-get:make)  echo build-essential ;;
        apt-get:makensis)         echo nsis ;;
        apt-get:x86_64-w64-mingw32-gcc) echo gcc-mingw-w64-x86-64 ;;
        apt-get:*)                echo "$2" ;;

        dnf:cc)                   echo gcc gcc-c++ ;;
        dnf:pkg-config)           echo pkgconf-pkg-config ;;
        # Fedora ships NSIS as part of its mingw stack, so the name looks
        # cross-ish; makensis in it is a native Linux binary all the same.
        dnf:makensis)             echo mingw32-nsis ;;
        dnf:x86_64-w64-mingw32-gcc) echo mingw64-gcc ;;
        dnf:*)                    echo "$2" ;;

        pacman:cc|pacman:make)    echo base-devel ;;
        pacman:pkg-config)        echo pkgconf ;;
        pacman:makensis)          echo nsis ;;
        pacman:x86_64-w64-mingw32-gcc) echo mingw-w64-gcc ;;
        pacman:*)                 echo "$2" ;;

        zypper:cc)                echo gcc gcc-c++ ;;
        # Neither NSIS nor the mingw toolchain is in the openSUSE distribution
        # repositories — both live in the windows:mingw OBS project, which is a
        # repository the user has to add and not something to do behind their
        # back. Hence no mapping.
        zypper:makensis|zypper:x86_64-w64-mingw32-gcc) ;;
        zypper:*)                 echo "$2" ;;
    esac
}

# Sets $sudo_cmd, and returns non-zero when the packages cannot be installed
# from here. $sudo_cmd is set either way, so the commands can still be printed
# for the user to run as root.
resolve_sudo() {
    if [ "$(id -u)" = 0 ]; then sudo_cmd=''; return 0; fi
    sudo_cmd=sudo
    need_cmd sudo
}

# Run or print one package-manager command. Both paths go through here so what
# --check reports cannot drift from what the install stage actually does.
pm_exec() {
    if [ "$mode" = run ] && [ "$can_install" = 1 ]; then
        say "$*"
        "$@"
    else
        printf '    %s\n' "$*"
    fi
}

# $sudo_cmd and $pkgs are deliberately unquoted: an empty sudo_cmd has to
# disappear rather than become an empty argument, and pkgs has to split.
pm_commands() {
    case "$pm" in
        apt-get)
            pm_exec $sudo_cmd env DEBIAN_FRONTEND=noninteractive apt-get update
            pm_exec $sudo_cmd env DEBIAN_FRONTEND=noninteractive apt-get install -y $pkgs
            ;;
        dnf)    pm_exec $sudo_cmd dnf install -y $pkgs ;;
        pacman) pm_exec $sudo_cmd pacman -S --needed --noconfirm $pkgs ;;
        zypper) pm_exec $sudo_cmd zypper --non-interactive install $pkgs ;;
    esac
}

ensure_system_deps() {
    check_deps
    [ -n "$missing" ] || return 0

    pm=''
    for candidate in apt-get dnf pacman zypper; do
        if need_cmd "$candidate"; then pm="$candidate"; break; fi
    done
    [ -n "$pm" ] || die "missing build dependencies:$missing (no apt-get, dnf, pacman or zypper here — install them with your distribution's tools)"

    pkgs=''
    unmapped=''
    for tool in $missing; do
        mapped="$(packages_for "$pm" "$tool")"
        if [ -n "$mapped" ]; then
            pkgs="$pkgs $mapped"
        else
            unmapped="$unmapped $tool"
        fi
    done
    pkgs="$(printf '%s\n' $pkgs | sort -u | tr '\n' ' ')"

    if [ -n "$unmapped" ]; then
        say "no package for$unmapped in this distribution's repositories — install it yourself, see the README"
        # Nothing left to install means there is nothing to say beyond that.
        if [ -z "$pkgs" ]; then
            if [ "$mode" = run ]; then die "cannot continue without$unmapped"; fi
            return 0
        fi
    fi

    can_install=1
    resolve_sudo || can_install=0

    if [ "$mode" != run ]; then
        printf 'Install with:\n'
        pm_commands
        return 0
    fi
    if [ "$can_install" = 0 ]; then
        printf 'build: missing build dependencies:%s\nRun these as root, then try again:\n' "$missing" >&2
        pm_commands >&2
        exit 1
    fi

    say "Installing build dependencies:$missing"
    pm_commands

    check_deps
    [ -z "$missing" ] || die "still missing after installing packages:$missing"
}

# ------------------------------------------------------------- stages ------

case "$(uname -s)" in
    Linux)
        ensure_system_deps
        ;;
    *)
        # Only Linux package managers are handled; elsewhere the C toolchain is
        # a manual step (on macOS: xcode-select --install). See the README.
        check_deps
        [ -z "$missing" ] || say "not Linux — install these yourself, see the README:$missing"
        ;;
esac

if [ "$mode" = check ]; then
    if have_cargo; then
        printf '    %s\n' "cargo: $(cargo --version)"
    else
        printf '    %s\n' "cargo: MISSING (rustup would install it)"
    fi
    [ -z "$missing" ] || exit 1
    say "Ready to build"
    exit 0
fi

ensure_rust

cd "$REPO_ROOT"

# The installer build has its own cargo invocation — a different target, its own
# staging and makensis - so there is nothing for the native build below to
# contribute, and no Linux binary to launch afterwards.
if [ "$want_installer" = 1 ]; then
    exec "$REPO_ROOT/packaging/build-installer.sh" "$@"
fi

cargo build --release -p quicksearch-gui

if [ "$do_run" = 0 ]; then
    say "Built target/release/quicksearch and target/release/quicksearch-cli"
    exit 0
fi

exec "$REPO_ROOT/target/release/quicksearch" "$@"
