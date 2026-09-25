//! OS keychain storage for the derived index key (hex), keyed by database
//! path. The password is never stored; the key lets a later unlock skip the
//! Argon2 derivation. All failures are non-fatal: callers fall back to asking
//! for the password.

use keyring::Entry;

const SERVICE: &str = "quicksearch";

fn entry(db_path: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, db_path).map_err(|e| format!("keychain unavailable: {}", e))
}

/// Remembers the derived key for this database on this machine.
fn store_key(db_path: &str, key_hex: &str) -> Result<(), String> {
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

/// Forgets the remembered key; an entry that never existed reports success.
/// A real failure means the key is still on the keychain, and callers must
/// surface that rather than record "not remembered".
pub fn delete_key(db_path: &str) -> Result<(), String> {
    match entry(db_path)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keychain delete failed: {}", e)),
    }
}

/// Makes the keychain match the preference: stores `key_hex` when given,
/// forgets the entry when not.
pub fn sync(db_path: &str, key_hex: Option<&str>) -> Result<(), String> {
    match key_hex {
        Some(hex) => store_key(db_path, hex),
        None => delete_key(db_path),
    }
}
