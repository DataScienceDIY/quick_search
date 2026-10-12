//! SQLite schema, on-disk open/recreate, and row-level repository helpers.
//! The indexer (owner) opens via [`open::open_or_recreate`], which wipes on
//! mismatch; *consumers* use [`open::open_existing`], which never wipes.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, InterruptHandle};

pub mod key;
pub mod open;
pub mod repo;
pub mod schema;

pub use key::{
    process_key_hex, set_hmac_mode_override, set_page_size_override, set_process_key,
    set_search_cache_override,
};
pub use open::{
    index_needs_rebuild, key_mismatch_parts, open_existing, open_or_recreate, remove_index_files,
    verify_process_key, KeyMismatch, CURRENT_SCHEMA_VERSION, FOREIGN_DB_PREFIX,
    KEY_MISMATCH_PREFIX,
};

/// Bumped whenever the index file is replaced rather than modified: a rebuild
/// or a clear puts a new file at the same path, and a long-lived connection
/// that misses the swap keeps serving the deleted inode. [`crate::search`]
/// compares this against the value it opened with.
/// [`open::remove_index_files`] bumps it once after every removal attempt, so
/// a reader that reopened while the delete was under way is caught too.
static INDEX_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The current index generation. See [`INDEX_EPOCH`].
pub fn index_epoch() -> u64 {
    INDEX_EPOCH.load(Ordering::SeqCst)
}

/// Declare that the index file has been replaced.
fn bump_index_epoch() {
    INDEX_EPOCH.fetch_add(1, Ordering::SeqCst);
    #[cfg(test)]
    EPOCH_BUMPS_HERE.with(|n| n.set(n.get() + 1));
}

#[cfg(test)]
thread_local! {
    /// Bumps made on this thread. Tests run in parallel against the one
    /// [`INDEX_EPOCH`], so a test counting its own bumps cannot read that.
    static EPOCH_BUMPS_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A [`hold_index`] callback.
type Release = Arc<dyn Fn(&Path) + Send + Sync>;

/// Whatever keeps the index open between uses, and how to make it let go.
static HOLDERS: Mutex<Vec<(u64, Release)>> = Mutex::new(Vec::new());
static NEXT_HOLDER: AtomicU64 = AtomicU64::new(0);

/// A [`hold_index`] registration; dropping it withdraws the callback.
pub struct IndexHolder(u64);

/// Register `release`, which [`open::remove_index_files`] calls with the path
/// it is about to delete, before it reads or deletes anything. It must close
/// every handle its owner holds on that path, and leave any other path alone.
/// Windows fails a delete while any handle is open, and a reader held for
/// minutes would otherwise fail every rebuild, a run's own wipe included.
pub fn hold_index(release: impl Fn(&Path) + Send + Sync + 'static) -> IndexHolder {
    let id = NEXT_HOLDER.fetch_add(1, Ordering::Relaxed);
    crate::lock_ok(&HOLDERS).push((id, Arc::new(release)));
    IndexHolder(id)
}

impl Drop for IndexHolder {
    fn drop(&mut self) {
        crate::lock_ok(&HOLDERS).retain(|(id, _)| *id != self.0);
    }
}

/// Run every [`hold_index`] callback for `path`, in registration order, and
/// outside the lock: a release waits on its owner's thread.
fn release_holders(path: &Path) {
    let holders: Vec<Release> = crate::lock_ok(&HOLDERS)
        .iter()
        .map(|(_, release)| release.clone())
        .collect();
    for release in holders {
        release(path);
    }
}

/// A shared slot holding the interrupt handle of whatever long statement is
/// running, so another thread can cut it short.
///
/// SQLite's interrupt is the only way out of a statement already running (a
/// stop flag only prevents the next one), so every cancellation is the pair
/// (flag, interrupt). An interrupted statement fails like a real error, so
/// callers re-read the flag on the error path to tell cancellation from
/// breakage.
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
pub fn interrupt(slot: &InterruptSlot) {
    if let Some(handle) = crate::lock_ok(slot).as_ref() {
        handle.interrupt();
    }
}
