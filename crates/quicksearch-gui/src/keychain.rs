//! OS keychain storage for the derived index key.
//!
//! Stores the *derived* SQLCipher key (hex), never the password: a
//! keychain unlock skips the ~0.5 s Argon2 derivation, and the password
//! itself never persists anywhere. Entries are keyed by database path so
//! portable installs and multiple profiles don't clobber each other.
//!
//! Every failure here is non-fatal by design — a missing Secret Service
//! daemon, a locked keyring, a denied prompt — and callers fall back to
//! asking for the password.

use keyring::Entry;

const SERVICE: &str = "quicksearch";

fn entry(db_path: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, db_path).map_err(|e| format!("keychain unavailable: {}", e))
}

/// Remember the derived key for this database on this machine.
pub fn store_key(db_path: &str, key_hex: &str) -> Result<(), String> {
    entry(db_path)?
        .set_password(key_hex)
        .map_err(|e| format!("keychain store failed: {}", e))
}

/// The remembered key, `Ok(None)` when nothing is stored.
pub fn load_key(db_path: &str) -> Result<Option<String>, String> {
    match entry(db_path)?.get_password() {
        Ok(hex) => Ok(Some(hex)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keychain read failed: {}", e)),
    }
}

/// Forget the remembered key. Best-effort: an entry that never existed or
/// a dead keychain daemon are both fine outcomes for "forget".
pub fn delete_key(db_path: &str) {
    if let Ok(entry) = entry(db_path) {
        let _ = entry.delete_credential();
    }
}
