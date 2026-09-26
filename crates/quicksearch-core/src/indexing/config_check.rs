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
    /// reconstruction cannot drift; `stored_config` parses these back.
    fn config_validation_entries(config: &Config, roots: &[String]) -> Vec<(&'static str, String)> {
        let sorted_joined = |v: &[String]| {
            let mut v: Vec<String> = v.to_vec();
            v.sort();
            v.join("\n")
        };
        vec![
            ("hash_length", config.processing.hash_length.to_string()),
            // Bump whenever the digest input changes, so old indexes are offered a rebuild.
            ("hash_algorithm", "size+head".to_string()),
            ("indexing_path", sorted_joined(roots)),
            ("tokenize", config.processing.tokenize.clone()),
            // The effective layout, not the flag: see `fts_positions`.
            (
                "fts_positions",
                config.processing.fts_positions().to_string(),
            ),
            ("include_hidden", config.indexing.include_hidden.to_string()),
            (
                "follow_symlinks",
                config.indexing.follow_symlinks.to_string(),
            ),
            (
                "ignore_patterns",
                sorted_joined(&config.indexing.ignore_patterns),
            ),
            (
                "content_extensions",
                sorted_joined(&config.indexing.content_extensions),
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

    /// Settings a difference in which cannot be reconciled: the FTS tokenizer
    /// and position setting are part of the table definition, and hashes
    /// under a different length or algorithm cannot be compared. Everything
    /// else reconciles; see [`crate::scope`].
    const REBUILD_KEYS: [&'static str; 4] =
        ["hash_length", "hash_algorithm", "tokenize", "fts_positions"];

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
    /// `interrupt` reaches the one in flight. Returns `false` if cut short,
    /// when nothing may be recorded as reconciled.
    pub(super) fn reconcile_stored_config(
        status: &Arc<Mutex<IndexingStatus>>,
        interrupt: &db::InterruptSlot,
        conn: &mut Connection,
        config: &Config,
        roots: &[String],
        stop_flag: &Arc<AtomicBool>,
    ) -> Result<bool, String> {
        let mut current = config.clone();
        current.paths.indexing_paths = roots.to_vec();

        let stored = crate::scope::stored_config(conn, &current)?;
        let work = crate::config::diff_actions(&stored, &current).work;
        if !work.touches_index() {
            return Ok(true);
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
            false
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
        Ok(true)
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
    /// satisfy: stamping those would clear a rebuild prompt the user declined.
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
