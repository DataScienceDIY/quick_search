#!/bin/sh
# Regenerate the committed LibreOffice fixtures from their sources.
#
# The .doc/.xls/.ppt cannot be built on the fly: `cfb` is the only Rust crate
# that writes OLE2 compound files, and `cfb` is what `extract::ole` reads them
# with; a fixture built by the reader's own library can only prove the two
# agree with each other. The .odt, .ods, .odp, .pptx and .rtf can be, and the
# corpus builds its own, but by hand from the spec, by the reader's author;
# these five are what a real producer writes. So a foreign producer writes
# all eight once, here, and the output is committed.
#
# Requires LibreOffice on PATH (Writer, Calc and Impress). The .doc, .xls and
# .ppt were written by 26.2.4.2, the other five by 26.2.6.3.
#
# Only the .doc, .xls, .ppt and .rtf are byte-reproducible (26.2.6.3 rewrites
# 26.2.4.2's three unchanged). LibreOffice stamps the time into each .odt,
# .ods, .odp and .pptx, so re-running this changes their bytes without
# changing the text: commit the result only when the *sources* changed.
# `cargo test -p quicksearch-core --test extraction_corpus` is what says the
# files are still right.
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

# Convert a copy of source $2 to format $1 as sample.$1, its needle $3
# replaced by $4: every needle must name exactly one file in the corpus.
renumbered() {
    mkdir "$work/$1"
    copy="$work/$1/sample.${2##*.}"
    sed "s/$3/$4/" "$here/$2" >"$copy"
    convert "$1" "$copy"
    mv "$work/sample.$1" "$here/sample.$1"
}

convert doc "$here/prose.txt"
convert xls "$here/sheet.csv"
convert ppt "$here/deck.fodp"

mv "$work/prose.doc" "$here/sample.doc"
mv "$work/sheet.xls" "$here/sample.xls"
mv "$work/deck.ppt"  "$here/sample.ppt"

renumbered odp  deck.fodp chalcedony9003 chalcedony9004
renumbered pptx deck.fodp chalcedony9003 chalcedony9005
renumbered odt  prose.txt chalcedony9001 chalcedony9006
renumbered rtf  prose.txt chalcedony9001 chalcedony9007
renumbered ods  sheet.csv chalcedony9002 chalcedony9008

ls -l "$here"/sample.*
