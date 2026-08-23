//! Process-global SQLCipher key, resolved once at startup before any
//! connection exists.

use std::sync::RwLock;

use crate::security::IndexKey;

static PROCESS_KEY: RwLock<Option<IndexKey>> = RwLock::new(None);

/// Install (or clear, with `None`) the key used by every subsequent
/// database open in this process.
pub fn set_process_key(key: Option<IndexKey>) {
    // Poison recovery matches `crate::lock_ok`'s policy: whole-value
    // replacement, so a panicked writer leaves the last fully-written value.
    *PROCESS_KEY.write().unwrap_or_else(|e| e.into_inner()) = key;
}

/// Snapshot of the current key for a single open.
pub(crate) fn process_key() -> Option<IndexKey> {
    PROCESS_KEY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Hex form of the installed key, if any; used by the GUI's keychain
/// "remember" toggle, which stores the derived key — never the password.
pub fn process_key_hex() -> Option<String> {
    PROCESS_KEY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|k| k.to_hex())
}
