//! Byte-for-byte verification that a set of files really is identical.
//!
//! The index groups duplicates by `sha256(size ‖ first hash_length bytes)`
//! (see [`crate::file_handling`]), which reads a file's head and nothing else
//! — a deliberate trade, since hashing every byte on a disk is most of the
//! cost of indexing it. Files of the same size whose heads agree are therefore
//! listed as duplicates whether or not they are: a fixed-size VHD keeps what
//! makes it unique in a footer, and a freshly pre-allocated disk image is
//! zeroes as far as the head can see. This turns that advisory grouping into
//! an answer, for the moment before someone deletes something.
//!
//! No hashing here, by policy. A digest per file would be shorter code and the
//! same answer nearly always — but "nearly always" is what the head hash
//! already offers, and the whole point of asking a second time is that this
//! time the bytes are compared.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Total read-buffer memory, split across the files being compared. A group is
/// usually two files and can be thousands — a hardlink farm, which is exactly
/// what `[indexing] ignore_patterns` warns about — so a per-file buffer of any
/// fixed size would become the largest allocation the process ever makes.
const CHUNK_BUDGET: usize = 8 * 1024 * 1024;
const MIN_CHUNK: usize = 16 * 1024;
const MAX_CHUNK: usize = 256 * 1024;

/// How often progress is emitted. Each one repaints the UI, and a chunk off a
/// warm page cache takes microseconds.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// What one member turned out to be, against the reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberVerdict {
    /// Same length, every byte agreed. The reference itself reads this.
    Identical,
    /// Offset of the first byte that disagreed.
    DiffersAt(u64),
    /// Lengths disagree, so nothing was read. Within a duplicate group this
    /// can only mean a stale index — the hash covers the size.
    LengthDiffers { len: u64, reference_len: u64 },
    /// Could not be opened, or stopped being readable part way through.
    Unreadable(String),
}

impl MemberVerdict {
    pub fn is_identical(&self) -> bool {
        matches!(self, MemberVerdict::Identical)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Index into the input paths of the file everything else was compared
    /// against: the first one that opened. `None` when none of them did.
    pub reference: Option<usize>,
    /// One verdict per input path, in the input order.
    pub verdicts: Vec<MemberVerdict>,
    /// Bytes actually read from disk, across every file.
    pub bytes_read: u64,
}

impl VerifyReport {
    /// Whether every member was read and matched. An empty or single-file set
    /// is vacuously identical.
    pub fn all_identical(&self) -> bool {
        self.verdicts.iter().all(MemberVerdict::is_identical)
    }

    pub fn differing(&self) -> usize {
        self.verdicts.iter().filter(|v| !v.is_identical()).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyUpdate {
    Progress { bytes_read: u64, bytes_total: u64 },
    Done(VerifyReport),
    Cancelled,
}

/// A file still in the running, with its own read buffer.
struct Live {
    index: usize,
    file: File,
    buf: Vec<u8>,
}

/// Compare every path against the first one that opens, byte for byte, and
/// report what each turned out to be.
///
/// Emits `Progress` while it works and exactly one terminal update — `Done`,
/// or `Cancelled` if `cancel` went up before the comparison finished.
pub fn verify_identical(paths: &[PathBuf], cancel: &AtomicBool, on: &mut dyn FnMut(VerifyUpdate)) {
    // Checked before the files are even opened, so a run cancelled before it
    // starts reports the cancellation rather than a verdict nobody waited for.
    if cancel.load(Ordering::Relaxed) {
        on(VerifyUpdate::Cancelled);
        return;
    }
    let mut verdicts = vec![MemberVerdict::Identical; paths.len()];

    // The reference is the first path that both opens *and* stats, not simply
    // the first path: one unreadable member must not cost the answer about all
    // the others.
    let mut reference: Option<(usize, File, u64)> = None;
    let mut rest: Vec<(usize, File)> = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        // These paths come from the index, which records what each file was
        // when it was walked. A member replaced by a FIFO since then needs no
        // race at all to be sitting here — the walk keeps the old row when a
        // regular file turns into something else — and a blocking open would
        // strand this worker before it reported a single verdict.
        let file = match crate::platform::open_regular_file(path) {
            Ok(f) => f,
            Err(e) => {
                verdicts[i] = MemberVerdict::Unreadable(describe(path, &e));
                continue;
            }
        };
        if reference.is_some() {
            rest.push((i, file));
            continue;
        }
        match file.metadata() {
            Ok(m) => reference = Some((i, file, m.len())),
            Err(e) => verdicts[i] = MemberVerdict::Unreadable(describe(path, &e)),
        }
    }

    let Some((reference, mut reference_file, reference_len)) = reference else {
        on(VerifyUpdate::Done(VerifyReport {
            reference: None,
            verdicts,
            bytes_read: 0,
        }));
        return;
    };

    // A length mismatch is decided from the handles, before a byte is read.
    let mut live: Vec<Live> = Vec::with_capacity(rest.len());
    for (i, file) in rest {
        match file.metadata() {
            Ok(m) if m.len() != reference_len => {
                verdicts[i] = MemberVerdict::LengthDiffers {
                    len: m.len(),
                    reference_len,
                };
            }
            Ok(_) => live.push(Live {
                index: i,
                file,
                buf: Vec::new(),
            }),
            Err(e) => verdicts[i] = MemberVerdict::Unreadable(describe(&paths[i], &e)),
        }
    }

    let chunk = (CHUNK_BUDGET / (live.len() + 1)).clamp(MIN_CHUNK, MAX_CHUNK);
    let mut reference_buf = vec![0u8; chunk];
    for l in live.iter_mut() {
        l.buf = vec![0u8; chunk];
    }

    let bytes_total = match live.len() {
        0 => 0,
        n => reference_len.saturating_mul(n as u64 + 1),
    };
    let mut bytes_read = 0u64;
    let mut offset = 0u64;
    // Backdated so the first chunk reports: a progress bar that only appears
    // after the first interval reads as a frozen window on a slow disk, which
    // is the case this is for.
    let mut last_progress = Instant::now()
        .checked_sub(PROGRESS_INTERVAL)
        .unwrap_or_else(Instant::now);

    while !live.is_empty() {
        if cancel.load(Ordering::Relaxed) {
            on(VerifyUpdate::Cancelled);
            return;
        }

        // Termination is driven by what the reference actually reads rather
        // than by the length it claimed, so a file truncated underneath us
        // degrades to a short comparison instead of a hang or a false match.
        let n = match read_chunk(&mut reference_file, &mut reference_buf) {
            Ok(0) => break, // EOF: everything still live matched all the way.
            Ok(n) => n,
            Err(e) => {
                verdicts[reference] = MemberVerdict::Unreadable(describe(&paths[reference], &e));
                // Survivors agreed up to here but cannot be finished. Saying
                // so is the only honest answer; "identical" would not be.
                for l in live.iter() {
                    verdicts[l.index] = MemberVerdict::Unreadable(format!(
                        "compared only to byte {offset}: {} could not be read to the end",
                        paths[reference].display()
                    ));
                }
                break;
            }
        };
        bytes_read += n as u64;

        let mut i = 0;
        while i < live.len() {
            let (got, verdict) = {
                let l = &mut live[i];
                match read_chunk(&mut l.file, &mut l.buf[..n]) {
                    Ok(m) => {
                        let common = n.min(m);
                        if let Some(k) =
                            first_difference(&reference_buf[..common], &l.buf[..common])
                        {
                            (m, Some(MemberVerdict::DiffersAt(offset + k as u64)))
                        } else if m < n {
                            // Same length a moment ago, shorter now.
                            (
                                m,
                                Some(MemberVerdict::Unreadable(format!(
                                    "{}: ended at byte {} while the file it was compared \
                                     against had more",
                                    paths[l.index].display(),
                                    offset + m as u64
                                ))),
                            )
                        } else {
                            (m, None)
                        }
                    }
                    Err(e) => (
                        0,
                        Some(MemberVerdict::Unreadable(describe(&paths[l.index], &e))),
                    ),
                }
            };
            bytes_read += got as u64;
            match verdict {
                Some(v) => {
                    verdicts[live[i].index] = v;
                    live.swap_remove(i);
                }
                None => i += 1,
            }
        }
        offset += n as u64;

        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            on(VerifyUpdate::Progress {
                bytes_read,
                bytes_total,
            });
        }
    }

    on(VerifyUpdate::Done(VerifyReport {
        reference: Some(reference),
        verdicts,
        bytes_read,
    }));
}

/// Fill `buf` as far as the file allows, returning how much. Short reads are
/// resumed and `Interrupted` retried, the way `extract::plaintext` does, so a
/// short return really does mean end of file.
fn read_chunk(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Offset of the first byte that differs. The equality test comes first
/// because it is a `memcmp`; the byte walk only ever runs on the one chunk
/// that turned out to differ.
fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    if a == b {
        return None;
    }
    Some(
        a.iter()
            .zip(b.iter())
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len())),
    )
}

fn describe(path: &Path, e: &std::io::Error) -> String {
    format!("{}: {}", path.display(), e)
}

#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
