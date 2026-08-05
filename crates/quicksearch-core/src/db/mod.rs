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
