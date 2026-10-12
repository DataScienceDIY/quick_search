//! The stored-config fingerprint: recording what a run was built
//! under, and validating/reconciling a later config against it.

use rusqlite::{params, Connection, OptionalExtension};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::Config;
use crate::db;
use crate::extract::Registry;

use super::*;

impl IndexingService {
    /// One list drives [`Self::validate_config`], [`Self::update_config`] and
    /// [`crate::scope::stored_config`], so record, comparison and
    /// reconstruction cannot drift; `stored_config` parses these back. It
    /// holds every setting a stored row was decided under: whether the file
    /// is walked (roots, hidden files, ignore patterns), whether its text is
    /// read (`content_extensions`, `maximum_text_file_size`), and what its
    /// hash and postings cover. The code behind those decisions (digest, MIME
    /// detection, extractors) is versioned by [`db::CURRENT_SCHEMA_VERSION`].
    fn config_validation_entries(config: &Config, roots: &[String]) -> Vec<(&'static str, String)> {
        let sorted_joined = |v: &[String]| {
            let mut v: Vec<String> = v.to_vec();
            v.sort();
            v.join("\n")
        };
        vec![
            ("hash_length", config.processing.hash_length.to_string()),
            ("indexing_path", sorted_joined(roots)),
            // The layout, as `schema_info` records it: the flag's opposite.
            (
                "fts_positions",
                (!config.processing.compact_text_index).to_string(),
            ),
            ("include_hidden", config.indexing.include_hidden.to_string()),
            (
                "ignore_patterns",
                sorted_joined(&config.indexing.ignore_patterns),
            ),
            (
                "content_extensions",
                sorted_joined(&config.indexing.content_extensions),
            ),
            (
                "maximum_text_file_size",
                config.processing.maximum_text_file_size.to_string(),
            ),
        ]
    }

    /// Every recorded `config_validation` key, for [`crate::scope::stored_config`].
    pub(crate) fn stored_validation(conn: &Connection) -> Result<Vec<(String, String)>, String> {
        let mut stmt = conn
            .prepare("SELECT key, value FROM config_validation")
            .map_err(|e| format!("prepare config_validation read: {}", e))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| format!("read config_validation: {}", e))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read config_validation row: {}", e))
    }

    /// Settings a difference in which cannot be reconciled: the FTS position
    /// setting is part of the table definition, and hashes under a different
    /// length cannot be compared. Both are enforced at open too:
    /// [`db::open_or_recreate`] wipes an index built under other values, so a
    /// declined rebuild happens at the next run. Everything else reconciles;
    /// see [`crate::scope`].
    const REBUILD_KEYS: [&'static str; 2] = ["hash_length", "fts_positions"];

    /// The unreconcilable settings that no longer match, which a rebuild
    /// prompt shows; `None` when none. A key absent (older index) never counts
    /// as changed.
    pub(super) fn validate_config(
        conn: &Connection,
        config: &Config,
        roots: &[String],
    ) -> Result<Option<Vec<ConfigChange>>, String> {
        let mut changes = Vec::new();
        for (key, current) in Self::config_validation_entries(config, roots)
            .into_iter()
            .filter(|(key, _)| Self::REBUILD_KEYS.contains(key))
        {
            let stored: Option<String> = conn
                .query_row(
                    "SELECT value FROM config_validation WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| format!("read config_validation.{}: {}", key, e))?;
            if let Some(stored) = stored {
                if stored != current {
                    changes.push(ConfigChange {
                        key: key.to_string(),
                        stored,
                        current,
                    });
                }
            }
        }
        Ok((!changes.is_empty()).then_some(changes))
    }

    /// Bring the index into line with `config` before a run walks anything.
    /// Runs in slices, so the stop flag is read between statements and
    /// `interrupt` reaches the one in flight. Returns the roots to stamp:
    /// `roots`, plus every recorded root while one of them is offline (see
    /// [`crate::scope::hold_offline_roots`]), so the record keeps an offline
    /// root's canonical spelling. `None` if cut short, when nothing may be
    /// recorded as reconciled.
    pub(super) fn reconcile_stored_config(
        status: &Arc<Mutex<IndexingStatus>>,
        interrupt: &db::InterruptSlot,
        conn: &mut Connection,
        config: &Config,
        roots: &[String],
        stop_flag: &Arc<AtomicBool>,
    ) -> Result<Option<Vec<String>>, String> {
        let mut current = config.clone();
        current.paths.indexing_paths = roots.to_vec();
        crate::scope::hold_offline_roots(conn, &mut current)?;
        let effective: Vec<String> = current.normalized_indexing_paths().into_iter().collect();

        let stored = crate::scope::stored_config(conn, &current)?;
        let work = crate::config::diff_actions(&stored, &current).work;
        if !work.touches_index() {
            return Ok(Some(effective));
        }
        crate::log_info!(
            "configuration changed since the last run: reconciling the index ({})",
            work.summary()
        );
        let registry = Registry::default_set();
        let mut cursor = crate::scope::WorkCursor::new(work, &current)?;
        let interrupted = |cursor: &crate::scope::WorkCursor| {
            crate::log_info!(
                "configuration reconcile interrupted after {} index entries; \
                 the next run starts it again",
                cursor.progress().examined
            );
            None
        };
        while !cursor.done() {
            if stop_flag.load(Ordering::Relaxed) {
                return Ok(interrupted(&cursor));
            }
            let outcome = {
                // Armed per slice, so the handle names only this scan's statement.
                let _armed = db::InterruptGuard::arm(interrupt, conn);
                crate::scope::advance(
                    conn,
                    &current,
                    &registry,
                    &mut cursor,
                    Instant::now() + crate::scope::SLICE,
                    stop_flag,
                )
            };
            if let Err(e) = outcome {
                // An interrupted statement fails like any other; ask the flag, not the message.
                if stop_flag.load(Ordering::Relaxed) {
                    return Ok(interrupted(&cursor));
                }
                return Err(e);
            }
            Self::set_prep_step(status, PrepStep::Reconciling(cursor.progress()));
        }
        if cursor.deleted > 0 || cursor.recontented > 0 {
            crate::log_info!(
                "configuration changed since the last run: {} index entries removed, \
                 {} re-examined for text extraction",
                cursor.deleted,
                cursor.recontented
            );
        }
        Ok(Some(effective))
    }

    /// Stamp the index with the settings it's being built under.
    pub(super) fn update_config(
        conn: &Connection,
        config: &Config,
        roots: &[String],
    ) -> Result<(), String> {
        Self::stamp(conn, Self::config_validation_entries(config, roots))
    }

    /// Stamp everything except [`Self::REBUILD_KEYS`], which a scan cannot
    /// satisfy: stamping those would cancel a rebuild the user declined, which
    /// the next run still owes.
    pub(crate) fn stamp_reconciled(
        conn: &Connection,
        config: &Config,
        roots: &[String],
    ) -> Result<(), String> {
        Self::stamp(
            conn,
            Self::config_validation_entries(config, roots)
                .into_iter()
                .filter(|(key, _)| !Self::REBUILD_KEYS.contains(key)),
        )
    }

    fn stamp(
        conn: &Connection,
        entries: impl IntoIterator<Item = (&'static str, String)>,
    ) -> Result<(), String> {
        for (key, current) in entries {
            conn.execute(
                "INSERT OR REPLACE INTO config_validation (key, value) VALUES (?1, ?2)",
                params![key, current],
            )
            .map_err(|e| format!("store config_validation.{}: {}", key, e))?;
        }
        Ok(())
    }
}
