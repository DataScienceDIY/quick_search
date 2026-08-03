//! Process-global SQLCipher key.
//!
//! Exactly two processes ever open the index (the GUI and the one-shot
//! terminal search), and each resolves the key once at startup — before any
//! connection exists — then never changes it except when the GUI
//! enables/disables protection (which tears down and rebuilds the index
//! anyway). A set-once global therefore reaches all open sites, several of
//! which only hold a `&str` path, without threading a parameter through
//! every layer.

use std::sync::RwLock;

use crate::security::IndexKey;

static PROCESS_KEY: RwLock<Option<IndexKey>> = RwLock::new(None);

/// Install (or clear, with `None`) the key used by every subsequent
/// database open in this process.
pub fn set_process_key(key: Option<IndexKey>) {
    *PROCESS_KEY.write().expect("process key lock poisoned") = key;
}

/// Snapshot of the current key for a single open.
pub(crate) fn process_key() -> Option<IndexKey> {
    PROCESS_KEY.read().expect("process key lock poisoned").clone()
}

/// Hex form of the installed key, if any. Exists for exactly one consumer:
/// the GUI's "remember on this device" toggle, which stores the derived
/// key (never the password) in the OS keychain.
pub fn process_key_hex() -> Option<String> {
    PROCESS_KEY
        .read()
        .expect("process key lock poisoned")
        .as_ref()
        .map(|k| k.to_hex())
}
