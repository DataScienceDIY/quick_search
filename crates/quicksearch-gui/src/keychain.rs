//! OS keychain storage for the derived index key, keyed by database path.
//! The password is never stored; the key lets a later unlock skip the
//! Argon2 derivation. A failed read falls back to the password prompt; a
//! failed store, or a failed delete of a key that was remembered, is reported
//! in the error banner.
//!
//! An entry is `<salt_hex>:<key_hex>`, the key with the salt it was derived
//! under. Every password change makes a new salt, so a key that a change
//! failed to store over reads as stale, even where no index exists to try
//! it against (`unlock::try_keychain_unlock`), rather than standing in for
//! the new password.

use keyring::Entry;
use quicksearch_core::security::{salt_from_hex, IndexKey, SALT_LEN};
use zeroize::Zeroizing;

const SERVICE: &str = "quicksearch";

fn entry(db_path: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, db_path).map_err(|e| format!("keychain unavailable: {}", e))
}

/// What to remember for `key_hex`, derived under the salt `salt_hex`.
pub fn entry_text(salt_hex: &str, key_hex: &str) -> Zeroizing<String> {
    // Sized up front, so no reallocation leaves a copy of the key behind.
    let mut text = Zeroizing::new(String::with_capacity(salt_hex.len() + 1 + key_hex.len()));
    text.push_str(salt_hex);
    text.push(':');
    text.push_str(key_hex);
    text
}

/// The key the entry `text` remembers, if it was derived under `salt`. An
/// entry from before the salt was recorded, or from under an earlier one, is
/// stale.
pub fn remembered_key(text: &str, salt: &[u8; SALT_LEN]) -> Result<IndexKey, String> {
    let Some((salt_hex, key_hex)) = text.split_once(':') else {
        return Err("stale: it predates recording the salt".to_string());
    };
    if salt_from_hex(salt_hex).ok() != Some(*salt) {
        return Err("stale: it was remembered for an earlier password".to_string());
    }
    IndexKey::from_hex(key_hex)
}

/// Remembers `text`, an [`entry_text`], for this database on this machine.
fn store_entry(db_path: &str, text: &str) -> Result<(), String> {
    entry(db_path)?
        .set_password(text)
        .map_err(|e| format!("keychain store failed: {}", e))
}

/// The remembered entry, `Ok(None)` when nothing is stored. Read the key out
/// of it with [`remembered_key`].
pub fn load_entry(db_path: &str) -> Result<Option<Zeroizing<String>>, String> {
    match entry(db_path)?.get_password() {
        Ok(text) => Ok(Some(Zeroizing::new(text))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keychain read failed: {}", e)),
    }
}

/// Forgets the remembered key; an entry that never existed reports success.
fn delete_key(db_path: &str) -> Result<(), String> {
    match entry(db_path)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keychain delete failed: {}", e)),
    }
}

/// Makes the keychain match the preference: stores `remembered`, an
/// [`entry_text`], when given, forgets the entry when not. A failed delete is
/// an error only when `was_remembered`, since the key is then still on the
/// keychain and callers must surface that rather than record "not
/// remembered". Otherwise the delete is best-effort and silent: it retries
/// any earlier failed delete, and a missing keychain service must not trouble
/// a user who never opted in.
pub fn sync(db_path: &str, was_remembered: bool, remembered: Option<&str>) -> Result<(), String> {
    match remembered {
        Some(text) => store_entry(db_path, text),
        None => match delete_key(db_path) {
            Err(e) if was_remembered => Err(e),
            _ => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quicksearch_core::security::salt_to_hex;

    const SALT: [u8; SALT_LEN] = [0x5a; SALT_LEN];
    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn an_entry_gives_back_its_key_under_its_own_salt() {
        let text = entry_text(&salt_to_hex(&SALT), KEY);
        assert_eq!(*text, format!("{}:{}", "5a".repeat(SALT_LEN), KEY));
        let key = remembered_key(&text, &SALT).expect("the salt matches");
        assert_eq!(key.to_hex(), KEY);
        // The salt is compared as bytes, not as text.
        let upper = entry_text(&salt_to_hex(&SALT).to_uppercase(), KEY);
        assert_eq!(
            remembered_key(&upper, &SALT).map(|k| k.to_hex()),
            Ok(KEY.to_string())
        );
    }

    /// A key left behind by a password change whose store failed carries
    /// the old salt, and one from before salts were recorded carries none:
    /// both are refused rather than standing in for the new password.
    #[test]
    fn an_entry_under_another_salt_or_none_is_stale() {
        let earlier = entry_text(&salt_to_hex(&[0xa5; SALT_LEN]), KEY);
        let refusal = remembered_key(&earlier, &SALT).unwrap_err();
        assert!(refusal.contains("earlier password"), "{refusal}");
        let refusal = remembered_key(KEY, &SALT).unwrap_err();
        assert!(refusal.contains("predates"), "{refusal}");
        let salt = salt_to_hex(&SALT);
        for bad in [
            format!("{salt}:"),
            format!("{salt}:not hex"),
            format!("{salt}:{}", &KEY[2..]),
            format!("{salt}:{KEY}:{KEY}"),
            format!(":{KEY}"),
            format!("{}:{KEY}", &salt[2..]),
            format!("zz{}:{KEY}", &salt[2..]),
        ] {
            assert!(remembered_key(&bad, &SALT).is_err(), "{bad:?}");
        }
    }
}
