//! Shared measurement plumbing for the `examples/*probe.rs` binaries.
//!
//! A `mod.rs` one directory down is not an auto-discovered target, so this
//! compiles only as a module of whichever probe writes `mod common;`.
//! Everything here reads the machine, not the library; none of it ships.

// The unused half is not dead code; it is another binary's.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Drop a tree — and optionally an index — from the page cache: without
/// this every read figure is `0.0 MiB` and anything measured about
/// traversal or read cost is measuring memory. `fadvise(DONTNEED)` needs
/// no root and evicts only this tree, but leaves dentry/inode caches warm;
/// on virtiofs/NFS the host cache survives, so "cold" there is a lower bound.
#[cfg(unix)]
pub fn evict(tree: &Path, db: Option<&Path>) -> (usize, u64) {
    use std::os::unix::io::AsRawFd;

    // Dirty pages cannot be dropped; flush before asking.
    unsafe { libc::sync() };

    fn drop_one(path: &Path) -> Option<u64> {
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        // (0, 0) means "to end of file".
        let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        (rc == 0).then_some(len)
    }

    fn walk(dir: &Path, files: &mut usize, bytes: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => walk(&path, files, bytes),
                Ok(ft) if ft.is_file() => {
                    if let Some(len) = drop_one(&path) {
                        *files += 1;
                        *bytes += len;
                    }
                }
                _ => {}
            }
        }
    }

    let (mut files, mut bytes) = (0usize, 0u64);
    walk(tree, &mut files, &mut bytes);
    if let Some(db) = db {
        for suffix in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{}", db.display(), suffix));
            if let Some(len) = drop_one(&p) {
                files += 1;
                bytes += len;
            }
        }
    }
    (files, bytes)
}

/// No equivalent worth having off Unix.
#[cfg(not(unix))]
pub fn evict(_tree: &Path, _db: Option<&Path>) -> (usize, u64) {
    eprintln!("evict: not supported on this platform");
    (0, 0)
}

/// The kernel's accounting from `/proc/self/io`: `read_bytes` is what
/// reached the block layer (the disk, not the page cache), and `syscr`
/// separates "read a lot" from "read a little, many times". Zero on
/// filesystems that don't report it; the caller says so.
#[derive(Default, Clone, Copy)]
pub struct Io {
    pub rchar: u64,
    pub wchar: u64,
    pub syscr: u64,
    pub syscw: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled: u64,
}

impl Io {
    pub fn read() -> Io {
        let mut io = Io::default();
        let Ok(text) = std::fs::read_to_string("/proc/self/io") else {
            return io;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let Ok(value) = value.trim().parse::<u64>() else {
                continue;
            };
            match key {
                "rchar" => io.rchar = value,
                "wchar" => io.wchar = value,
                "syscr" => io.syscr = value,
                "syscw" => io.syscw = value,
                "read_bytes" => io.read_bytes = value,
                "write_bytes" => io.write_bytes = value,
                "cancelled_write_bytes" => io.cancelled = value,
                _ => {}
            }
        }
        io
    }

    pub fn since(&self, start: &Io) -> Io {
        Io {
            rchar: self.rchar.saturating_sub(start.rchar),
            wchar: self.wchar.saturating_sub(start.wchar),
            syscr: self.syscr.saturating_sub(start.syscr),
            syscw: self.syscw.saturating_sub(start.syscw),
            read_bytes: self.read_bytes.saturating_sub(start.read_bytes),
            write_bytes: self.write_bytes.saturating_sub(start.write_bytes),
            cancelled: self.cancelled.saturating_sub(start.cancelled),
        }
    }
}

pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Peak RSS from the kernel's own `VmHWM` — unlike a sampled figure it
/// cannot miss a spike; `None` where `/proc` is absent.
pub fn vm_hwm() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}
