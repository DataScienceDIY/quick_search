# Legacy Office fixtures

`sample.doc`, `sample.xls` and `sample.ppt`, written by LibreOffice and
committed. Read by `tests/extraction_corpus.rs` through the corpus module in
`tests/corpus/legacy.rs`.

## Why these three are committed when the rest of the corpus is generated

Every other format in the extraction corpus is written at test time by a
library that is *not* the one that reads it back — the whole point being that a
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

The unit tests in `src/extract/ole_tests.rs` are the complement, not a
duplicate: they build deliberately malformed streams to check bounds and error
paths, which needs byte-level control that only `cfb` gives.

## Files

| file | role |
|---|---|
| `prose.txt` | source for `sample.doc` |
| `sheet.csv` | source for `sample.xls` — one cell per row, no commas |
| `deck.fodp` | source for `sample.ppt` — flat ODF, so the text is reviewable |
| `sample.doc` `sample.xls` `sample.ppt` | LibreOffice output, committed |
| `regen.sh` | regenerates the three from the sources |

The sources are the source of truth: `legacy.rs` reads the expected fragments
out of them rather than restating the text, so a regenerated fixture that
dropped a line fails the test instead of quietly redefining what it should
contain. Each source carries a needle (`chalcedony9001`-`chalcedony9003`) that
the end-to-end search looks for; `legacy.rs` asserts it is still present.

## Regenerating

```sh
./regen.sh          # needs libreoffice on PATH; last run with 26.2.4.2
cargo test -p quicksearch-core --test extraction_corpus
```

The output is **not** byte-reproducible — LibreOffice stamps a creation time
into each file — so re-running changes the bytes without changing the text.
Commit the result only when the *sources* changed; the test is what says the
files are still right.

`sample.ppt` is ~460 KB because the PowerPoint 97 export filter embeds the
master slide. There is no filter option that trims it, and it is well inside
the 2 MiB `maximum_text_file_size` the end-to-end run indexes with.

Unlike the generated half of the corpus, the text in these three is fixed:
`QUICKSEARCH_CORPUS_SEED` does not affect them.
