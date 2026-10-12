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

Performance is measured, not guessed: benchmarks and probes measure indexing
and search speed, memory churn, peak memory and disk I/O.

## Storage

The index is a single SQLite database: file metadata, an FTS5 full-text index
over document contents, and the extracted text itself, stored zstd-compressed
for snippets, ranking and verification. The database runs in WAL mode with exactly one
writer at a time, so read-only searches are never blocked by indexing. Schema
changes wipe and rebuild the index rather than migrating: the index is a cache
of the filesystem, so rebuilding is always safe, and only the indexer may do
it. Nothing deletes the file at the configured database path unless it is
empty, recognisably an index, or indistinguishable from an encrypted one, and a
file another program holds open is never deleted.
FTS5 runs in trigram mode for full-text search and also speeds up regex
and fuzzy searches. By default it records no word positions: every candidate
is checked against the stored text anyway, so positions only narrow the
candidates, and dropping them makes the index several times smaller and
faster to build, for content searches up to two or three times slower. A
setting trades the other way. Tuning this database is central to QuickSearch's
performance.

## The search path never stalls

Searches answer from the index and never wait on the filesystem or the
indexer. Each keystroke cancels the previous query. Results stream in strongest
pass first; within a pass they arrive as found, so a better match found later
can land above weaker ones. A small watcher then checks the visible rows
against the disk and hands changed files to the indexer; a vanished row is
shown as gone, and only the index's file watcher or the next run removes it.

## Indexing

Each indexed root gets its own directory walker and content-extraction pool,
but all database writes go through one thread and one connection. That writer
serves the walkers first and slices extraction work, so one root's large
documents never stall another root's progress. Changing what is indexed does
not throw the index away: the configuration difference becomes a plan to
delete what fell out of scope and walk what is newly covered, applied while
searching and indexing keep running. A root that cannot be reached, or that
lists as empty because nothing is mounted on it, never counts as removed or
emptied; its entries wait for it to come back. The app parses each PDF,
audio file and legacy Office document (doc, xls, ppt) in a child process
under CPU and memory limits, so a hostile file that would overflow a
third-party parser's stack or allocate without bound fails alone.

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
Encryption is for confidentiality at rest: a stolen disk, a synced backup or a
copied index reveals neither file names nor contents, and also prevents
non-authorized users or apps from having a faster way to find sensitive files
than scanning the originals. It does not protect against software running as the
user, which could read the original files anyway. SQLCipher's per-page HMAC, which
only detects tampering, is off because tampering with a search tool is low-threat
and it costs significant search performance.

Turning protection on does not securely erase the unencrypted index it replaces.
The file is simply deleted and parts can remain recoverable in freed disk space.
Changing the password leaves the old encrypted index behind the same way, still
readable with the old password.

## Deliberate omissions

- Files whose names are not valid UTF-8 are silently skipped. A lossily
  converted path must never become a database key, and the corner case does
  not justify UI.
- Symbolic links and junctions inside an indexed folder are not followed. To
  index what one points at, add the target as its own folder; a folder chosen
  as a root may itself be a link.
- A file counts as unchanged while its size and its modified time, to the
  whole second, match the index. A same-size rewrite within one second, or a
  same-size replacement that keeps the old time, is missed until the file
  changes again. Sub-second times would cost a rebuild and gain nothing on
  FAT, which keeps two-second times, and the size already catches appends
  and most replacements.
- Duplicate groups are a strong suspicion, not a certainty: grouping hashes
  only each file's size and head, which the walk reads anyway to detect the
  file's type, so it costs little. The Duplicates tab compares files byte for
  byte on request and never deletes or modifies them; it tells you where to
  look. Hard links and bind-mounted copies of one file group as copies, and
  deleting one frees nothing; verification calls them identical, which they
  are.
- Only one instance may open an index for writing where the filesystem
  supports locks. A second launch raises the running window instead, and
  refuses if it cannot rather than risk corruption; where the filesystem
  cannot lock, it raises an instance that answers and otherwise starts
  anyway. The guard is a kernel lock, so a crash never leaves you locked out.
- The app bundles one text face and one monospace face, nothing more. CJK,
  Arabic, Hebrew, Thai, Indic scripts and emoji come from the system's own
  fonts, found after the first frame, and show as boxes where it has none.
  egui does no shaping or right-to-left layout, so Arabic letters show
  unjoined, Thai and Indic marks sit out of place, and Arabic and Hebrew run
  left to right.

## Where the detail lives

Tests live in each crate; benchmarks and profiling probes in quicksearch-core
(`cargo bench`, `examples/`). Release and CI mechanics are
documented in `.forgejo/workflows/ci.yml` and the scripts under `packaging/`.
