//! Byte-for-byte verification that a set of files really is identical — the
//! answer behind the index's advisory head-hash duplicate grouping, for the
//! moment before someone deletes something. No hashing here, by policy: the
//! whole point of asking a second time is that the bytes are compared.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Total read-buffer memory, split across the files being compared: a group
/// can be thousands of files (a hardlink farm), so a fixed per-file buffer
/// would become the largest allocation the process ever makes.
const CHUNK_BUDGET: usize = 8 * 1024 * 1024;
const MIN_CHUNK: usize = 16 * 1024;
const MAX_CHUNK: usize = 256 * 1024;

/// How often progress is emitted; each one repaints the UI.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberVerdict {
    /// The reference itself reads this.
    Identical,
    /// Offset of the first byte that disagreed.
    DiffersAt(u64),
    /// Lengths disagree, so nothing was read. Within a duplicate group this
    /// can only mean a stale index — the hash covers the size.
    LengthDiffers { len: u64, reference_len: u64 },
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
    pub verdicts: Vec<MemberVerdict>,
    pub bytes_read: u64,
}

impl VerifyReport {
    /// An empty or single-file set is vacuously identical.
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

struct Live {
    index: usize,
    file: File,
    buf: Vec<u8>,
}

/// Compare every path against the first one that opens, byte for byte.
/// Emits `Progress` while it works and exactly one terminal update.
pub fn verify_identical(paths: &[PathBuf], cancel: &AtomicBool, on: &mut dyn FnMut(VerifyUpdate)) {
    if cancel.load(Ordering::Relaxed) {
        on(VerifyUpdate::Cancelled);
        return;
    }
    let mut verdicts = vec![MemberVerdict::Identical; paths.len()];

    // The reference is the first path that both opens *and* stats: one
    // unreadable member must not cost the answer about all the others.
    let mut reference: Option<(usize, File, u64)> = None;
    let mut rest: Vec<(usize, File)> = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        // A member replaced by a FIFO since the walk would strand this
        // worker on a blocking open before it reported a single verdict.
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
    // after the first interval reads as a frozen window on a slow disk.
    let mut last_progress = Instant::now()
        .checked_sub(PROGRESS_INTERVAL)
        .unwrap_or_else(Instant::now);

    while !live.is_empty() {
        if cancel.load(Ordering::Relaxed) {
            on(VerifyUpdate::Cancelled);
            return;
        }

        // Termination is driven by what the reference actually reads, so a
        // file truncated underneath us degrades to a short comparison
        // instead of a hang or a false match.
        let n = match read_chunk(&mut reference_file, &mut reference_buf) {
            Ok(0) => break, // EOF: everything still live matched all the way.
            Ok(n) => n,
            Err(e) => {
                verdicts[reference] = MemberVerdict::Unreadable(describe(&paths[reference], &e));
                // Survivors agreed up to here but cannot be finished.
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

/// Fill `buf` as far as the file allows. Short reads are resumed and
/// `Interrupted` retried, so a short return really does mean end of file.
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
/// because it is a `memcmp`.
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
