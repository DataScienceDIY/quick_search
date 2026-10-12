//! Byte-for-byte verification that a set of files is identical: the definite
//! answer behind the index's advisory head-hash duplicate grouping, for the
//! moment before someone deletes something. No hashing, by policy: asking a
//! second time is worth it only because the bytes are compared.

use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Read-buffer memory for one batch, split evenly across the reference and
/// the batch's members: chunk = CHUNK_BUDGET / (batch + 1), at most
/// [`MAX_CHUNK`]. Only one batch is read at a time, so the bound holds
/// whatever the group size (a hardlink farm can be thousands of files).
const CHUNK_BUDGET: usize = 8 * 1024 * 1024;
const MAX_CHUNK: usize = 256 * 1024;

/// Members held open at once, besides the reference. Descriptors are a
/// per-process limit (RLIMIT_NOFILE is 1024 on Linux and often 256 on
/// macOS) shared with search, SQLite and a concurrent run's extraction, so
/// a large group is compared a batch at a time. It also floors the chunk:
/// CHUNK_BUDGET / (MAX_OPEN + 1) is about 126 KiB.
const MAX_OPEN: usize = 64;

/// How often progress is emitted; each one repaints the UI.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberVerdict {
    /// The reference itself reads this.
    Identical,
    /// Offset of the first byte that disagreed.
    DiffersAt(u64),
    /// Lengths disagree, so nothing was read. Within a duplicate group this
    /// can only mean a stale index, since the hash covers the size.
    LengthDiffers {
        len: u64,
        reference_len: u64,
    },
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
    /// How long that reference was, so a report can say *where* in the file a
    /// difference landed. 0 when there was no reference to measure.
    pub reference_len: u64,
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

/// Compare every path against the first one that opens, byte for byte. The
/// rest are compared in input order, in batches of at most [`MAX_OPEN`],
/// each batch reading the reference again from its start. Emits `Progress`
/// while it works and exactly one terminal update.
pub fn verify_identical(paths: &[PathBuf], cancel: &AtomicBool, on: &mut dyn FnMut(VerifyUpdate)) {
    if cancel.load(Ordering::Relaxed) {
        on(VerifyUpdate::Cancelled);
        return;
    }
    let mut verdicts = vec![MemberVerdict::Identical; paths.len()];

    // The reference is the first path that opens: one unreadable member must
    // not cost the answer about all the others. Its handle stays open for
    // the whole run, and the length its `fstat` gave is the one every member
    // must have. `open_regular_file`, here and per batch: a member replaced
    // by a FIFO since the walk would strand this worker on a blocking open.
    let mut reference = None;
    for (i, path) in paths.iter().enumerate() {
        match crate::platform::open_regular_file(path) {
            Ok((file, meta)) => {
                reference = Some((i, file, meta.len()));
                break;
            }
            Err(e) => verdicts[i] = MemberVerdict::Unreadable(describe(path, &e)),
        }
    }

    let Some((reference, mut reference_file, reference_len)) = reference else {
        on(VerifyUpdate::Done(VerifyReport {
            reference: None,
            reference_len: 0,
            verdicts,
            bytes_read: 0,
        }));
        return;
    };

    // Every member once and the reference once per batch, less what opening
    // decides: a member whose length differs or that will not open reads
    // nothing, nor does the reference for a batch left with no member. Exact
    // unless a member differs or the reference fails part way.
    let members = paths.len() - reference - 1;
    let mut bytes_total =
        reference_len.saturating_mul((members + members.div_ceil(MAX_OPEN)) as u64);
    let mut bytes_read = 0u64;
    // Backdated so the first chunk reports: a progress bar that only appears
    // after the first interval reads as a frozen window on a slow disk.
    let mut last_progress = Instant::now()
        .checked_sub(PROGRESS_INTERVAL)
        .unwrap_or_else(Instant::now);

    for start in (reference + 1..paths.len()).step_by(MAX_OPEN) {
        let end = (start + MAX_OPEN).min(paths.len());
        // A length mismatch is decided from the handles, before a byte is
        // read, and its handle is closed at once.
        let mut live: Vec<Live> = Vec::with_capacity(end - start);
        for i in start..end {
            match crate::platform::open_regular_file(&paths[i]) {
                Ok((_, meta)) if meta.len() != reference_len => {
                    verdicts[i] = MemberVerdict::LengthDiffers {
                        len: meta.len(),
                        reference_len,
                    };
                    bytes_total = bytes_total.saturating_sub(reference_len);
                }
                Ok((file, _)) => live.push(Live {
                    index: i,
                    file,
                    buf: Vec::new(),
                }),
                Err(e) => {
                    verdicts[i] = MemberVerdict::Unreadable(describe(&paths[i], &e));
                    bytes_total = bytes_total.saturating_sub(reference_len);
                }
            }
        }
        if live.is_empty() {
            bytes_total = bytes_total.saturating_sub(reference_len);
        }

        let chunk = (CHUNK_BUDGET / (live.len() + 1)).min(MAX_CHUNK);
        let mut reference_buf = vec![0u8; chunk];
        for l in live.iter_mut() {
            l.buf = vec![0u8; chunk];
        }

        // Set when the reference fails or changes: why every member the run
        // never reached is not Identical either.
        let mut stop: Option<String> = None;
        let mut offset = 0u64;
        while !live.is_empty() {
            if cancel.load(Ordering::Relaxed) {
                on(VerifyUpdate::Cancelled);
                return;
            }

            // The reference is read only up to the length the members were
            // matched against: where it ends within that drives termination,
            // and growth past it shows in its handle's length.
            let want = (chunk as u64).min(reference_len - offset) as usize;
            // Each batch reads the reference from its start; a failed seek is
            // a failed read.
            let read = match offset {
                0 => reference_file.rewind(),
                _ => Ok(()),
            }
            .and_then(|()| read_chunk(&mut reference_file, &mut reference_buf[..want]));
            let n = match read {
                // The reference ended, or reached the length it had. It
                // changed if it ended early or its handle now says another
                // length; a survivor changed if its handle does. This arm
                // must end the batch: falling through as a read of 0 bytes
                // would loop forever.
                Ok(0) => {
                    if offset != reference_len || !has_len(&reference_file, reference_len) {
                        let why = changed(&paths[reference]);
                        verdicts[reference] = MemberVerdict::Unreadable(why.clone());
                        for l in &live {
                            verdicts[l.index] = MemberVerdict::Unreadable(why.clone());
                        }
                        stop = Some(why);
                    } else {
                        for l in live.iter().filter(|l| !has_len(&l.file, offset)) {
                            verdicts[l.index] = MemberVerdict::Unreadable(changed(&paths[l.index]));
                        }
                    }
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    verdicts[reference] =
                        MemberVerdict::Unreadable(describe(&paths[reference], &e));
                    // Survivors agreed up to here but cannot be finished.
                    for l in live.iter() {
                        verdicts[l.index] = MemberVerdict::Unreadable(format!(
                            "compared only to byte {offset}: {} could not be read to the end",
                            paths[reference].display()
                        ));
                    }
                    stop = Some(format!(
                        "not compared: {} could not be read to the end",
                        paths[reference].display()
                    ));
                    break;
                }
            };
            bytes_read += n as u64;

            // A member that reaches a verdict stops being read; the verdict
            // goes in by index, so the order `live` keeps does not matter.
            live.retain_mut(|l| {
                let verdict = match read_chunk(&mut l.file, &mut l.buf[..n]) {
                    Ok(m) => {
                        bytes_read += m as u64;
                        let common = n.min(m);
                        if let Some(k) =
                            first_difference(&reference_buf[..common], &l.buf[..common])
                        {
                            Some(MemberVerdict::DiffersAt(offset + k as u64))
                        } else if m < n {
                            // Same length a moment ago, shorter now.
                            Some(MemberVerdict::Unreadable(format!(
                                "{}: ended at byte {} while the file it was compared \
                                 against had more",
                                paths[l.index].display(),
                                offset + m as u64
                            )))
                        } else {
                            None
                        }
                    }
                    Err(e) => Some(MemberVerdict::Unreadable(describe(&paths[l.index], &e))),
                };
                match verdict {
                    Some(v) => {
                        verdicts[l.index] = v;
                        false
                    }
                    None => true,
                }
            });
            offset += n as u64;

            if last_progress.elapsed() >= PROGRESS_INTERVAL {
                last_progress = Instant::now();
                on(VerifyUpdate::Progress {
                    bytes_read,
                    bytes_total,
                });
            }
        }

        // No member the run never reached keeps the Identical default.
        if let Some(why) = stop {
            for v in &mut verdicts[end..] {
                *v = MemberVerdict::Unreadable(why.clone());
            }
            break;
        }
    }

    on(VerifyUpdate::Done(VerifyReport {
        reference: Some(reference),
        reference_len,
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

/// Whether the handle still says `len` bytes. A failed `fstat` counts as a
/// change: only a length it confirms can back an Identical.
fn has_len(file: &File, len: u64) -> bool {
    file.metadata().is_ok_and(|m| m.len() == len)
}

fn changed(path: &Path) -> String {
    format!("{} changed while it was being compared", path.display())
}

fn describe(path: &Path, e: &std::io::Error) -> String {
    format!("{}: {}", path.display(), e)
}

#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
