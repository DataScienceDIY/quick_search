//! SQLite schema, on-disk open/recreate, and row-level repository helpers.
//!
//! Policy: the indexer (owner) opens via [`open::open_or_recreate`], which on
//! any schema mismatch — wrong version, drifted tokenizer, absent
//! `schema_info` — wipes the DB and rebuilds from [`schema::SCHEMA_CURRENT`].
//! There are no in-place migrations by design; re-indexing is accepted as the
//! cost of avoiding migration-path complexity. *Consumers* (search, status,
//! size, `clear`) instead use [`open::open_existing`], which never creates or
//! wipes — a tokenizer difference or stale version is an error, not data loss.

pub mod key;
pub mod open;
pub mod repo;
pub mod schema;

pub use key::{process_key_hex, set_process_key};
pub use open::{
    open_existing, open_or_recreate, verify_process_key, CURRENT_SCHEMA_VERSION,
    KEY_MISMATCH_PREFIX,
};
