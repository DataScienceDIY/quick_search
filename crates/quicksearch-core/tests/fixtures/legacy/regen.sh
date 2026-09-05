#!/bin/sh
# Regenerate the committed .doc/.xls/.ppt fixtures from their sources.
#
# These three are the one corner of the extraction corpus that cannot be built
# on the fly: `cfb` is the only Rust crate that writes OLE2 compound files, and
# `cfb` is what `extract::ole` reads them with — a fixture built by the reader's
# own library can only prove the two agree with each other. So a foreign
# producer writes them once, here, and the output is committed.
#
# Requires LibreOffice on PATH. Last run with 26.2.4.2.
#
# The output is NOT byte-reproducible: LibreOffice stamps a creation time into
# each file. Re-running this changes the bytes without changing the text, so
# commit the result only when the *sources* changed. `cargo test -p
# quicksearch-core --test extraction_corpus` is what says the files are still
# right.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

convert() {
    # -env:UserInstallation keeps this off the invoking user's LibreOffice
    # profile, which also lets it run when a desktop LibreOffice is open.
    libreoffice --headless \
        -env:UserInstallation="file://$work/profile" \
        --convert-to "$1" --outdir "$work" "$2" >/dev/null
}

convert doc "$here/prose.txt"
convert xls "$here/sheet.csv"
convert ppt "$here/deck.fodp"

mv "$work/prose.doc" "$here/sample.doc"
mv "$work/sheet.xls" "$here/sample.xls"
mv "$work/deck.ppt"  "$here/sample.ppt"

ls -l "$here/sample.doc" "$here/sample.xls" "$here/sample.ppt"
