#!/usr/bin/env bash
# One-time system setup for building and running the Dioxus-based
# `quicksearch-gui` binary on Debian / Ubuntu. The core library
# (`quicksearch-core`) has no native system deps and doesn't need this.
#
# Usage:
#     ./setup.sh                # installs everything
#
# Adds `pkg-config` and `build-essential` explicitly because the Dioxus
# build scripts (glib-sys, gdk-sys, gio-sys, javascriptcoregtk-sys,
# webkit2gtk-sys) shell out to `pkg-config` and will refuse to build
# without it. Some minimal Ubuntu flavors and container images don't
# install pkg-config transitively.
set -e

sudo apt update
sudo apt install -y \
    pkg-config \
    build-essential \
    libsoup-3.0-dev \
    libjavascriptcoregtk-4.1-dev \
    libwebkit2gtk-4.1-dev \
    libxdo-dev
