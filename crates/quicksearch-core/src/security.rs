//! Password → SQLCipher key derivation for the optional index encryption.
//!
//! The chain is small: `key = Argon2id(password, salt)`, used directly as
//! the SQLCipher raw key. The salt is 16 random bytes generated once, when a
//! password is set, and stored as hex in the config file — it exists to make
//! the derivation unique per install, not to be secret. Argon2id is what
//! makes offline brute-force expensive; SQLCipher's own KDF is bypassed
//! (raw-key form) so the cost is paid once per unlock, not per connection.
//!
//! Callers own password hygiene: hold the raw password in a
//! [`zeroize::Zeroizing`] buffer, call [`derive_key`], and drop the buffer
//! immediately. Nothing in this module stores or logs the password.

use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Length in bytes of the per-install KDF salt stored (hex) in the config.
pub const SALT_LEN: usize = 16;
/// Length in bytes of the derived SQLCipher raw key.
pub const KEY_LEN: usize = 32;

/// Argon2id cost parameters. Changing any of these changes every derived
/// key, which makes every protected index unreadable — treat them like a
/// schema version. ~0.5 s on desktop hardware; paid once per unlock, never
/// per connection (the keychain path skips it entirely).
const ARGON2_MEM_KIB: u32 = 64 * 1024;
const ARGON2_ITERS: u32 = 3;
const ARGON2_LANES: u32 = 1;

/// The derived SQLCipher key. Zeroed on drop; `Debug` is redacted so it can
/// never leak through logs or error formatting.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct IndexKey([u8; KEY_LEN]);

impl std::fmt::Debug for IndexKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IndexKey(<redacted>)")
    }
}

impl IndexKey {
    /// Lowercase hex, suitable for SQLCipher's raw-key `PRAGMA key = "x'…'"`
    /// form and for keychain storage.
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    /// Strict inverse of [`IndexKey::to_hex`]: exactly 64 hex digits, any case.
    pub fn from_hex(hex: &str) -> Result<IndexKey, String> {
        let bytes = hex_decode(hex)?;
        let arr: [u8; KEY_LEN] = bytes
            .try_into()
            .map_err(|_| format!("key must be {} hex digits", KEY_LEN * 2))?;
        Ok(IndexKey(arr))
    }
}

/// Fresh random salt. Called only when a password is being set — a salt is
/// never invented anywhere else (no default exists).
pub fn generate_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).expect("OS randomness unavailable");
    salt
}

pub fn salt_to_hex(salt: &[u8; SALT_LEN]) -> String {
    hex_encode(salt)
}

/// Strict decode of a config-stored salt: exactly 32 hex digits. Anything
/// else — wrong length, non-hex bytes, a hand-crafted oversized value — is
/// an error, never silently truncated or padded.
pub fn salt_from_hex(hex: &str) -> Result<[u8; SALT_LEN], String> {
    let bytes = hex_decode(hex)?;
    bytes
        .try_into()
        .map_err(|_| format!("salt must be {} hex digits", SALT_LEN * 2))
}

/// Derive the SQLCipher key from a password and the per-install salt.
/// Deterministic: same inputs always yield the same key.
pub fn derive_key(password: &str, salt: &[u8; SALT_LEN]) -> IndexKey {
    let params = Params::new(ARGON2_MEM_KIB, ARGON2_ITERS, ARGON2_LANES, Some(KEY_LEN))
        .expect("static Argon2 params are valid");
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .expect("Argon2 accepts any password with a fixed-size salt");
    IndexKey(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        write!(s, "{:02x}", b).expect("writing to a String cannot fail");
    }
    s
}

fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("hex string has odd length".to_string());
    }
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("hex string contains non-hex characters".to_string());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector: pins password + salt → key. If this test breaks, the
    /// derivation changed and every existing protected index just became
    /// unreadable — that must never happen by accident.
    #[test]
    fn derive_key_is_pinned() {
        let salt = salt_from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
        let key = derive_key("correct horse battery staple", &salt);
        assert_eq!(
            key.to_hex(),
            "0d1a3c6523c8f06e4e0af9c515aa5b5448cfebd6838f2d52c3d8b6ef8ddc3c2e"
        );
    }

    #[test]
    fn derive_key_is_deterministic_and_salt_sensitive() {
        let salt_a = salt_from_hex("000102030405060708090a0b0c0d0e0f").unwrap();
        let salt_b = salt_from_hex("ffffffffffffffffffffffffffffffff").unwrap();
        assert_eq!(derive_key("pw", &salt_a), derive_key("pw", &salt_a));
        assert_ne!(derive_key("pw", &salt_a), derive_key("pw", &salt_b));
        assert_ne!(derive_key("pw", &salt_a), derive_key("pw2", &salt_a));
    }

    #[test]
    fn empty_and_unicode_passwords_derive() {
        // SQLCipher accepts any 32-byte key; the password's content is the
        // user's business, including empty or emoji.
        let salt = generate_salt();
        let _ = derive_key("", &salt);
        let _ = derive_key("på55wörd 🗝️", &salt);
    }

    #[test]
    fn generated_salts_are_unique_and_sized() {
        let a = generate_salt();
        let b = generate_salt();
        assert_eq!(a.len(), SALT_LEN);
        assert_ne!(a, b, "two fresh salts must not collide");
    }

    #[test]
    fn salt_hex_round_trips() {
        let salt = generate_salt();
        let hex = salt_to_hex(&salt);
        assert_eq!(hex.len(), SALT_LEN * 2);
        assert_eq!(salt_from_hex(&hex).unwrap(), salt);
        // Uppercase input decodes too.
        assert_eq!(salt_from_hex(&hex.to_uppercase()).unwrap(), salt);
    }

    #[test]
    fn malformed_salts_are_rejected() {
        // Too short / too long / odd length / non-hex — all hostile-config
        // shapes, all hard errors.
        assert!(salt_from_hex("").is_err());
        assert!(salt_from_hex("abcd").is_err());
        assert!(salt_from_hex(&"ab".repeat(SALT_LEN + 1)).is_err());
        assert!(salt_from_hex(&"ab".repeat(SALT_LEN * 64)).is_err());
        assert!(salt_from_hex("0g0102030405060708090a0b0c0d0e0f").is_err());
        assert!(salt_from_hex("00010203040506070809 a0b0c0d0e0f").is_err());
        let odd = "000102030405060708090a0b0c0d0e0";
        assert!(salt_from_hex(odd).is_err());
    }

    #[test]
    fn key_hex_round_trips_and_rejects_malformed() {
        let salt = generate_salt();
        let key = derive_key("pw", &salt);
        let hex = key.to_hex();
        assert_eq!(hex.len(), KEY_LEN * 2);
        assert_eq!(IndexKey::from_hex(&hex).unwrap(), key);
        assert!(IndexKey::from_hex("").is_err());
        assert!(IndexKey::from_hex("abcd").is_err());
        assert!(IndexKey::from_hex(&"ab".repeat(KEY_LEN + 1)).is_err());
        assert!(IndexKey::from_hex(&hex[..hex.len() - 1]).is_err());
        assert!(IndexKey::from_hex(&format!("zz{}", &hex[2..])).is_err());
    }

    #[test]
    fn debug_output_is_redacted() {
        let key = derive_key("secret", &generate_salt());
        let dbg = format!("{:?}", key);
        assert_eq!(dbg, "IndexKey(<redacted>)");
        assert!(!dbg.contains(&key.to_hex()));
    }
}
