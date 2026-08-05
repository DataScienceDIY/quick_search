//! SQLite schema, on-disk open/recreate, and row-level repository helpers.
//!
//! Policy: the indexer (owner) opens via [`open::open_or_recreate`], which on
//! any schema mismatch — wrong version, drifted tokenizer, absent
//! `schema_info` — wipes the DB and rebuilds from [`schema::SCHEMA_CURRENT`].
//! There are no in-place migrations by design; re-indexing is accepted as the
//! cost of avoiding migration-path complexity. *Consumers* (search, status,
//! size, `clear`) instead use [`open::open_existing`], which never creates or
//! wipes — a tokenizer difference or stale version is an error, not data loss.

use std::sync::Mutex;

use rusqlite::{Connection, InterruptHandle};

pub mod key;
pub mod open;
pub mod repo;
pub mod schema;

pub use key::{process_key_hex, set_process_key};
pub use open::{
    index_needs_rebuild, open_existing, open_or_recreate, verify_process_key,
    CURRENT_SCHEMA_VERSION, KEY_MISMATCH_PREFIX,
};

/// Bumped whenever the index file is replaced rather than modified.
///
/// Process-wide, like [`key::process_key`], and for the same reason: it is a
/// fact about the index this process is working with, not about any one
/// connection to it.
///
/// Anything holding a connection open across operations needs to know that the
/// file underneath it has been swapped, and the path cannot tell it — a
/// rebuild and a clear both put a *new* file at the *same* path. A connection
/// that missed the change keeps serving the deleted inode: stale results, and
/// on Linux the old file's blocks stay allocated for as long as the handle
/// lives. [`crate::search`] is the only long-lived reader today; it compares
/// this against the value it opened with.
static INDEX_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current index generation. See [`INDEX_EPOCH`].
pub fn index_epoch() -> u64 {
    INDEX_EPOCH.load(std::sync::atomic::Ordering::SeqCst)
}

/// Declare that the index file has been replaced.
///
/// Called from [`open::open_or_recreate`]'s wipe path — the one place a wipe
/// actually happens, so a schema drift nobody asked for cannot slip past —
/// and from the coordinator's explicit rebuild and clear commands, which
/// delete the file without going through it.
pub fn bump_index_epoch() {
    INDEX_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// A shared slot holding the interrupt handle of whatever long statement is
/// running, so another thread can cut it short.
///
/// # Why a flag is not enough
///
/// SQLite's own interrupt is the only way out of a statement already in
/// flight: a `DELETE` over a whole root's range, an FTS merge or a VACUUM
/// answer to nothing else, and on a large index each can run for minutes. A
/// flag can only stop the *next* statement from starting.
///
/// So cancelling anything long is two halves — the flag, which prevents the
/// next statement, and this, which ends the current one — and a caller that
/// wants a bounded wait needs both. Every user of this type is one of those
/// pairs: [`crate::scope::advance`] and its `cancel` argument,
/// `coordinator::ReconcileStop`, and the stop flag alongside
/// `IndexingService::cancel_db_work`. This is the reference statement of the
/// rule; those sites record only what it means locally.
///
/// One further consequence, which is why [`InterruptGuard`] exists rather than
/// a set/clear pair: an interrupted statement fails like any other and cannot
/// be told apart from a real failure by its error message. The flag the
/// canceller set is the only reliable answer, so callers re-read it on the
/// error path before deciding whether they were cancelled or broke.
pub type InterruptSlot = Mutex<Option<InterruptHandle>>;

/// Publish `conn`'s interrupt handle in `slot` for as long as this lives.
///
/// The guard, rather than a set/clear pair, because every path out matters:
/// a handle left behind after an early `?` would let a later interrupt —
/// aimed at the next long statement, or at a VACUUM — land on whatever that
/// connection happens to be running by then.
pub struct InterruptGuard<'a> {
    slot: &'a InterruptSlot,
}

impl<'a> InterruptGuard<'a> {
    pub fn arm(slot: &'a InterruptSlot, conn: &Connection) -> InterruptGuard<'a> {
        if let Ok(mut held) = slot.lock() {
            *held = Some(conn.get_interrupt_handle());
        }
        InterruptGuard { slot }
    }
}

impl Drop for InterruptGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut held) = self.slot.lock() {
            *held = None;
        }
    }
}

/// Interrupt the statement `slot` holds a handle for, if there is one.
/// A no-op otherwise, and on a connection that has since closed.
pub fn interrupt(slot: &InterruptSlot) {
    if let Ok(held) = slot.lock() {
        if let Some(handle) = held.as_ref() {
            handle.interrupt();
        }
    }
}
