# QuickSearch design notes

A brief record of the design choices and the reasoning behind them. The code
and its comments specify exact behavior.

## Shape of the codebase

`quicksearch-core` is the library (indexing, storage, search), and
`quicksearch-gui` is the app (an immediate-mode egui desktop). On Linux the
app binary also searches from a terminal; on Windows the separate
`quicksearch-cli` does. The backend is synchronous Rust: plain threads and
mpsc channels, no async runtime, because a small, fixed set of long-lived
workers is simpler as threads.

## Optimization

Performance is measured, not guessed: a benchmark suite tracks CPU time, peak
memory, memory churn, and indexing and search speed.

## Storage

The index is a single SQLite database: file metadata, an FTS5 full-text index
over document contents, and the extracted text itself, stored zstd-compressed
for snippets, ranking and verification. The database runs in WAL mode with exactly one
writer at a time, so read-only searches are never blocked by indexing. Schema
changes wipe and rebuild the index rather than migrating: the index is a cache
of the filesystem, so rebuilding is always safe, and only the indexer may do
it. FTS5 runs in trigram mode for full-text search and also speeds up regex
and fuzzy searches. By default it records no word positions: every candidate
is checked against the stored text anyway, so positions only narrow the
candidates, and dropping them makes the index several times smaller and
faster to build, for content searches up to two or three times slower. A
setting trades the other way. Tuning this database is central to QuickSearch's
performance.

## The search path never stalls

Searches answer from the index and never wait on the filesystem or the
indexer. Each keystroke cancels the previous query; results stream out in rank
order and weaker matches only append, so with the default rank sort nothing
you have seen reshuffles. A small watcher then checks the visible rows against
the disk and feeds any corrections to the indexer, which keeps results honest
even when indexing is stopped or periodic.

## Indexing

Each indexed root gets its own directory walker and content-extraction pool,
but all database writes go through one thread and one connection. That writer
serves the walkers first and slices extraction work, so one root's large
documents never stall another root's progress. Changing what is indexed does
not throw the index away: the configuration difference becomes a plan to
delete what fell out of scope and walk what is newly covered, applied while
searching and indexing keep running.

## Freshness

Filesystem watchers turn changes into small incremental updates. When roots
are too large for watchers to scale, a full reindex on a configurable interval
is the backstop. There is no background daemon and no "start with your
session" option: QuickSearch starts and indexes in moments, and a search tool
has no business running when it is not being used.

## Security

QuickSearch has no more permissions than any other user program, but by
gathering what it can read into one searchable place it concentrates the risk
of data theft. So the index can be encrypted on disk with SQLCipher, keyed
from a password through Argon2id; the OS keychain can remember the key.
SQLCipher's per-page HMAC is off: it only detects tampering (not reading) by
someone who could already read the indexed files, and it costs significant
search performance. Encryption protects against a stolen disk, a synced backup
or a copied index, not against software running as the user, which could read
the original files anyway.

## Deliberate omissions

- Files whose names are not valid UTF-8 are silently skipped. A lossily
  converted path must never become a database key, and the corner case does
  not justify UI.
- Duplicate groups are a strong suspicion, not a certainty: grouping hashes
  only each file's size and head, which the walk reads anyway to detect the
  file's type, so it costs little. The Duplicates tab compares files byte for
  byte on request and never deletes or modifies them; it tells you where to
  look.
- Only one instance may open an index for writing. A second launch raises the
  running window instead, and refuses if it cannot rather than risk
  corruption. The guard is a kernel lock, so a crash never leaves you locked
  out.

## Where the detail lives

Tests, benchmarks and profiling probes live in each crate (see `cargo test`
and `cargo bench` in the crate directories). Release and CI mechanics are
documented in `.forgejo/workflows/ci.yml` and the scripts under `packaging/`.
