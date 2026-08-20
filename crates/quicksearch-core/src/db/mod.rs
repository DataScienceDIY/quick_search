//! SQLite schema, on-disk open/recreate, and row-level repository helpers.
//!
//! Policy: the indexer (owner) opens via [`open::open_or_recreate`], which on
//! any schema mismatch wipes the DB and rebuilds from
//! [`schema::SCHEMA_CURRENT`]; there are no in-place migrations. *Consumers*
//! (search, status, size, `clear`) use [`open::open_existing`], which never
//! creates or wipes — a mismatch is an error, not data loss.

use std::sync::Mutex;

use rusqlite::{Connection, InterruptHandle};

pub mod key;
pub mod open;
pub mod repo;
pub mod schema;

pub use key::{process_key_hex, set_process_key};
pub use open::{
    index_needs_rebuild, key_mismatch_parts, open_existing, open_or_recreate, verify_process_key,
    KeyMismatch, CURRENT_SCHEMA_VERSION, KEY_MISMATCH_PREFIX,
};

/// Bumped whenever the index file is replaced rather than modified — a
/// rebuild and a clear both put a *new* file at the *same* path, so a
/// long-lived connection that misses the swap keeps serving the deleted
/// inode. [`crate::search`] compares this against the value it opened with.
static INDEX_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current index generation. See [`INDEX_EPOCH`].
pub fn index_epoch() -> u64 {
    INDEX_EPOCH.load(std::sync::atomic::Ordering::SeqCst)
}

/// Declare that the index file has been replaced.
pub fn bump_index_epoch() {
    INDEX_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// A shared slot holding the interrupt handle of whatever long statement is
/// running, so another thread can cut it short.
///
/// Protocol: SQLite's interrupt is the only way out of a statement already in
/// flight — a stop flag can only prevent the *next* one from starting — so
/// every cancellation is the pair (flag, interrupt). An interrupted statement
/// fails indistinguishably from a real error, so callers re-read the flag on
/// the error path to tell cancellation from breakage.
pub type InterruptSlot = Mutex<Option<InterruptHandle>>;

/// Publish `conn`'s interrupt handle in `slot` for as long as this lives.
pub struct InterruptGuard<'a> {
    slot: &'a InterruptSlot,
}

impl<'a> InterruptGuard<'a> {
    pub fn arm(slot: &'a InterruptSlot, conn: &Connection) -> InterruptGuard<'a> {
        *crate::lock_ok(slot) = Some(conn.get_interrupt_handle());
        InterruptGuard { slot }
    }
}

impl Drop for InterruptGuard<'_> {
    fn drop(&mut self) {
        *crate::lock_ok(self.slot) = None;
    }
}

/// Interrupt the statement `slot` holds a handle for, if there is one.
/// A no-op otherwise, and on a connection that has since closed.
pub fn interrupt(slot: &InterruptSlot) {
    if let Some(handle) = crate::lock_ok(slot).as_ref() {
        handle.interrupt();
    }
}
