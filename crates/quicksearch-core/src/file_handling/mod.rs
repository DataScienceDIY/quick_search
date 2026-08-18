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

pub(crate) use batch::max_text_file_size;
pub use batch::{
    cleanup_stale_index_entries, count_extract_scope, mark_oversize_pending_na,
    process_batch_inserts, process_batch_updates, store_extracted, ExtractCursor, ExtractScope,
    Stored,
};
pub use counting::count_tree_entries_fast;
pub use paths::{db_key_for_missing_path, filtered_dirs, filtered_walk, UnreadableDirs};
pub(crate) use paths::{normalize_root_string, path_to_db_string, warn_if_unrepresentable};
pub use records::{
    classify_by_mtime, classify_for_indexing, content_extractable, decide_content,
    extract_and_store, fts_finalize_after_text_indexing, hash_failure_counts, outcome_body,
    prepare_file_record, prepare_file_record_from_path, reset_run_warnings, store_content_outcome,
    ContentOutcome, DirRows, FileIndexAction, OwnedNewFile,
};
