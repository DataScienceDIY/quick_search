//! Process-global SQLCipher key, resolved once at startup before any
//! connection exists.

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
    PROCESS_KEY
        .read()
        .expect("process key lock poisoned")
        .clone()
}

/// Hex form of the installed key, if any; used by the GUI's keychain
/// "remember" toggle, which stores the derived key — never the password.
pub fn process_key_hex() -> Option<String> {
    PROCESS_KEY
        .read()
        .expect("process key lock poisoned")
        .as_ref()
        .map(|k| k.to_hex())
}
