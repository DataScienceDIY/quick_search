//! Per-file indexing steps: path normalization, tree counting, record
//! building and hashing, and the batched DB writes the indexer funnels into.

mod batch;
mod counting;
mod paths;
mod records;

#[cfg(test)]
mod count_and_extract_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use batch::extract_scope_prepare;
pub(crate) use batch::max_text_file_size;
pub use batch::{
    cleanup_stale_index_entries, count_extract_scope, mark_oversize_pending_na,
    process_batch_inserts, process_batch_updates, store_extracted, Bodies, ExtractCursor,
    ExtractScope, RootRange, Stored,
};
pub use counting::count_tree_entries_fast;
pub use paths::{
    db_key_for_missing_path, filtered_dirs, filtered_walk, split_db_path, UnreadableDirs,
};
pub(crate) use paths::{
    dir_to_db_parent, index_file_set, normalize_root_string, path_to_db_string,
    warn_if_unrepresentable, INDEX_SIDECAR_SUFFIXES,
};
pub(crate) use records::mtime_secs;
pub use records::{
    classify_by_mtime, classify_for_indexing, content_extractable, decide_content,
    extract_and_store, fts_begin_bulk_write, fts_begin_tombstone_burst, fts_end_tombstone_burst,
    fts_finalize_after_text_indexing, get_file_hash, hash_failure_counts, outcome_body,
    prepare_file_record, reset_run_warnings, ContentOutcome, DirRows, FileIndexAction,
    OwnedNewFile, FTS_DELETEMERGE,
};
