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
    /// The settings the index was built under, paired with their current
    /// values. One list drives [`validate_config`], [`update_config`] and
    /// [`crate::scope::stored_config`], so record, comparison and
    /// reconstruction cannot drift. List values are sorted before joining and
    /// the roots arrive canonicalized; `stored_config` parses these back.
    fn config_validation_entries(config: &Config, roots: &[String]) -> Vec<(&'static str, String)> {
        let sorted_joined = |v: &[String]| {
            let mut v: Vec<String> = v.to_vec();
            v.sort();
            v.join("\n")
        };
        vec![
            ("hash_length", config.processing.hash_length.to_string()),
            // Bump this string whenever the digest input changes, so existing
            // indexes are offered a rebuild instead of silently mixing
            // hash schemes.
            ("hash_algorithm", "size+head".to_string()),
            ("indexing_path", sorted_joined(roots)),
            ("tokenize", config.processing.tokenize.clone()),
            ("include_hidden", config.indexing.include_hidden.to_string()),
            // Decides whether symlink targets are in the index at all, so a
            // change leaves rows that no longer belong.
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
            (
                "store_text_for_snippets",
                config.processing.store_text_for_snippets.to_string(),
            ),
        ]
    }

    /// Every recorded `config_validation` key, for
    /// [`crate::scope::stored_config`] to rebuild the configuration the index
    /// was last written under.
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

    /// The recorded settings a difference in which cannot be reconciled: the
    /// FTS tokenizer is part of the table definition, and a hash written under
    /// a different length or algorithm cannot be compared with a new one.
    /// Everything else is recoverable — see [`crate::scope`].
    const REBUILD_KEYS: [&'static str; 3] = ["hash_length", "hash_algorithm", "tokenize"];

    /// The settings the index was built under that no longer match and cannot
    /// be reconciled — the list a rebuild prompt shows. `None` when there are
    /// none. A key absent from the DB (older index) never counts as changed.
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
        Ok(if changes.is_empty() {
            None
        } else {
            Some(changes)
        })
    }

    /// Bring the index into line with `config` before a run walks anything.
    ///
    /// A no-op in the normal case (the coordinator reconciled at edit time);
    /// it exists for configs changed while the app was not running. Runs to
    /// completion, but in slices, so the status moves and the stop flag is
    /// read between statements; `interrupt` reaches the statement in flight.
    ///
    /// Returns whether it finished: `false` means it was cut short and
    /// nothing may be recorded as reconciled.
    pub(super) fn reconcile_stored_config(
        status: &Arc<Mutex<IndexingStatus>>,
        interrupt: &db::InterruptSlot,
        conn: &mut Connection,
        config: &Config,
        roots: &[String],
        stop_flag: &Arc<AtomicBool>,
    ) -> Result<bool, String> {
        // What this run is about to index, which is `config` everywhere except
        // its roots — see the caller.
        let mut current = config.clone();
        current.paths.indexing_paths = roots.to_vec();

        let stored = crate::scope::stored_config(conn, &current)?;
        let work = crate::config::diff_actions(&stored, &current).work;
        if !work.touches_index() {
            return Ok(true);
        }
        // Announced ahead of the scan — the wait it describes can be minutes.
        crate::log_info!(
            "configuration changed since the last run: reconciling the index ({})",
            work.summary()
        );
        let registry = Registry::default_set();
        let mut cursor = crate::scope::WorkCursor::new(work, &current)?;
        while !cursor.done() {
            if stop_flag.load(Ordering::Relaxed) {
                crate::log_info!(
                    "configuration reconcile interrupted after {} index entries; \
                     the next run starts it again",
                    cursor.progress().examined
                );
                return Ok(false);
            }
            let outcome = {
                // Armed per slice, so the handle names only this scan's
                // statement; the interrupt reaches the one already in flight.
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
                // An interrupted statement fails like any other; ask the flag,
                // not the message. A stopped run is not an error.
                if stop_flag.load(Ordering::Relaxed) {
                    crate::log_info!(
                        "configuration reconcile interrupted after {} index entries; \
                         the next run starts it again",
                        cursor.progress().examined
                    );
                    return Ok(false);
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

    /// Record the settings a reconciliation has just brought the index into
    /// line with — everything except [`Self::REBUILD_KEYS`], which a scan
    /// cannot satisfy: stamping those would clear a rebuild prompt the user
    /// declined.
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
