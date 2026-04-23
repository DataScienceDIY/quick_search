//! SQLite schema, on-disk open/recreate, and row-level repository helpers.
//!
//! Policy: a single [`open::open_or_recreate`] is the only entry point. Any
//! schema mismatch — wrong version, drifted tokenizer, absent `schema_info`
//! — wipes the DB and rebuilds from [`schema::SCHEMA_CURRENT`]. There are
//! no in-place migrations by design; re-indexing is accepted as the cost
//! of avoiding migration-path complexity.

pub mod open;
pub mod repo;
pub mod schema;

pub use open::{open_or_recreate, CURRENT_SCHEMA_VERSION};
