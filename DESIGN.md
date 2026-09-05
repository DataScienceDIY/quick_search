# QuickSearch design notes

A brief record of the design choices and the reasoning behind them. Exact
behavior is specified by the code and its comments. This document covers architecture and design.

## Shape of codebase

`quicksearch-core` is the library (indexing, storage, search)
`quicksearch-gui` is the app binary (an immediate-mode egui desktop)
The built binary doubles as the GUI and a terminal search tool. 
The backend is synchronous Rust: plain threads and mpsc channels, 
no async runtime. The workloads are a small, fixed set of long-lived
workers, which threads express more simply than a runtime would.

## Optimization
A great deal of effort has gone into measuring and improving on 
performance and efficiency of the software. A benchmark suite which
includes cpu time, peak memory, memory churn, and indexing / search speed
is included for measuring and tracking these aspects.

## Storage

The index is a single SQLite database: file metadata, an FTS5 full-text
index over document contents, and the extracted text itself stored
zstd-compressed for snippets and ranking. The database runs in WAL mode
with exactly one writer at a time, so read-only searches are never blocked
by indexing. Schema changes wipe and rebuild the index rather than
migrating. The index is a cache of the filesystem, so rebuilding is always
safe, and only the indexer is allowed to do it. FTS5 is used in trigram mode
for efficient full text search and also used to accelerate regex
and fuzzy searches. Careful managment and tuning of this SQLite database
is central to the indexing and search performance of QuickSearch.

## The search path never stalls

Searching answers from the index, verifies cheaply, and repairs
asynchronously. Searches don't wait on the filesystem or the indexer. Each
keystroke interrupts the previous query, results stream out in rank order,
and weaker matches only ever append to the bottom of the list, so what you
have already seen never reshuffles (when sorting by rank as the default).
After the index results are retrieved, they are verified against the disk.
When the screen and the disk disagree, a small watcher re-checks the visible
rows against the disk and feeds corrections back to the indexer, which is
what keeps results honest even with indexing stopped or in periodic mode.

## Indexing

Each indexed root gets its own directory walker and content-extraction
pool, but all database writes funnel through one thread and one connection.
That writer serves the walkers first and slices extraction work, so one
root's large documents never stall another root's progress. Changing what is
indexed does not throw the index away: the configuration difference is
turned into a plan to delete what fell out of scope, and walk what is newly
covered; then that plan is applied while search or indexing keep running.

## Freshness

Filesystem watchers turn changes into small incremental updates. When
indexed roots are too large for filesystem watchers to scale well, a full
reindex runs on a configurable interval as the backstop. There is no
background daemon and no "start with your session" option: QuickSearch
starts and indexes in moments, and a search tool has no business running
when it is not being used.

## Security

QuickSearch has no special permissions compared to other user-space
programs, however it concentrates risk of data theft by both consolidating
and making easy to search what it can access. Accordingly, the index
can optionally be encrypted on disk with SQLCipher, keyed by
Argon2id-derived material from a password, and which the OS keychain can
remember. SQLCipher's per-page HMAC is deliberately disabled: it only
detects tampering (not reading) by someone who could already read 
the indexed files directly, and it costs significant search performance.
The threat model which encryption protects against is a stolen disk, a
synced backup, or a cloned index; not against software already running
with user permissions, but such software could read the original files
anyway.

## Deliberate omissions

- Files with names that are not valid UTF-8 are silently skipped. A
  lossily-converted path must never become a database key, and the corner
  case does not justify UI.
- Duplicate groups are a strong suspicion, not a certainty: grouping hashes
  only each file's size and head. Fast detection relies on iterating sorted
  hashes of the start of each file (which are prefetched on dir walk and
  cost minimal indexing time). The duplicate tool provides an on-demand
  byte-for-byte comparison, and the tool never deletes or modifies files —
  it tells you where to look.
- Only one instance may open an index for writing; a second launch refuses
  rather than risking corruption. The guard is a kernel lock, so a crash
  never leaves you locked out.

## Where the detail lives

Tests, benchmarks, and profiling probes live in each crate (see
`cargo test`/`cargo bench` in the crate directories). Release and CI
mechanics are documented in `.forgejo/workflows/ci.yml` and the scripts
under `packaging/`.
