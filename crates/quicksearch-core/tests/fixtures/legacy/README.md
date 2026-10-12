# LibreOffice fixtures

`sample.doc`, `sample.xls`, `sample.ppt`, `sample.odt`, `sample.ods`,
`sample.odp`, `sample.pptx` and `sample.rtf`, written by LibreOffice and
committed. Read by `tests/extraction_corpus.rs` through the corpus module in
`tests/corpus/legacy.rs`.

## Why these are committed when the rest of the corpus is generated

Every other format in the extraction corpus is written at test time by code
that is *not* the code that reads it back — the whole point being that a
fixture built with the reader's own library can only prove the two agree with
each other. The pre-2007 binary formats have no such library: `cfb` is the only
Rust crate that writes OLE2 compound files, and `cfb` is what `extract::ole`
reads them with.

So a foreign producer writes them once, here, and the output is committed.
That buys more than independence — a LibreOffice `.doc` is a real FIB with a
real piece table, its `.xls` a real BIFF stream with a real `SST`, and its
`.ppt` drags the master slide's placeholder prompts (`Click to edit the title
text format`, `___PPT10`) into the text stream alongside the content. None of
those shapes come out of a minimal synthetic fixture, and the last one is why
the corpus asserts ordered *containment* rather than equality.

The `.odt`, `.ods`, `.odp`, `.pptx` and `.rtf` are the same argument one
step removed. No Rust crate writes them either, so the corpus hand-assembles
its own from the spec (`tests/corpus/odf.rs`, `tests/corpus/ooxml.rs`,
`tests/corpus/rtf.rs`): independent of the reader's code, but written by the
reader's author from the same reading of the spec. LibreOffice's are a real
producer's: ODF packages with their styles, settings and thumbnails, a
`.pptx` with its master, layouts, theme and relationships, and an `.rtf`
with font, colour and style tables and each non-ASCII character as a `\uN`
escape followed by a `\'hh` fallback the reader must skip.

The unit tests in `src/extract/ole_tests.rs` are the complement, not a
duplicate: they build deliberately malformed streams to check bounds and error
paths, which needs byte-level control that only `cfb` gives.

## Files

| file | role |
|---|---|
| `prose.txt` | source for `sample.doc`, `sample.odt` and `sample.rtf` |
| `sheet.csv` | source for `sample.xls` and `sample.ods` — one cell per row, no commas |
| `deck.fodp` | source for `sample.ppt`, `sample.odp` and `sample.pptx` — flat ODF, so the text is reviewable |
| `sample.doc` `sample.xls` `sample.ppt` `sample.odt` `sample.ods` `sample.odp` `sample.pptx` `sample.rtf` | LibreOffice output, committed |
| `regen.sh` | regenerates the eight from the sources |

The sources are the source of truth: `legacy.rs` reads the expected fragments
out of them rather than restating the text, so a regenerated fixture that
dropped a line fails the test instead of quietly redefining what it should
contain. Each source carries a needle (`chalcedony9001`-`chalcedony9003`) that
the end-to-end search looks for; `legacy.rs` asserts it is still present. The
other five fixtures are converted from copies of their source whose needle
`regen.sh` renumbers (`chalcedony9004`-`chalcedony9008`), so every needle
still names exactly one file.

## Regenerating

```sh
./regen.sh          # needs libreoffice (Writer, Calc, Impress) on PATH
cargo test -p quicksearch-core --test extraction_corpus
```

The `.doc`, `.xls` and `.ppt` were written by 26.2.4.2, the other five by
26.2.6.3.

Only the `.doc`, `.xls`, `.ppt` and `.rtf` are byte-reproducible (26.2.6.3
rewrites 26.2.4.2's three unchanged). LibreOffice stamps the time into each
`.odt`, `.ods`, `.odp` and `.pptx`, so re-running changes their bytes without
changing the text. Commit the result only when the *sources* changed; the
test is what says the files are still right.

`sample.ppt` is ~460 KB because the PowerPoint 97 export filter embeds the
master slide. There is no filter option that trims it, and it is well inside
the 50 MiB `maximum_text_file_size` the end-to-end run indexes with.

Unlike the generated half of the corpus, the text in these eight is fixed:
`QUICKSEARCH_CORPUS_SEED` does not affect them.
