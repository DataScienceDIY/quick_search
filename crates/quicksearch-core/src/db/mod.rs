//! SQLite schema, migrations, and row-level repository helpers.
//!
//! The only "live" schema is `CURRENT_SCHEMA_VERSION` (see [`schema`]). Older
//! databases are detected in [`migrate::open_and_migrate`] and recreated from
//! scratch — Set A of the QuickSearch → Baloo work is the first schema bump
//! and carries no rows we'd want to preserve. Subsequent migrations should
//! prefer `ALTER TABLE` and versioned steps.

pub mod migrate;
pub mod repo;
pub mod schema;

pub use migrate::{open_and_migrate, CURRENT_SCHEMA_VERSION};
