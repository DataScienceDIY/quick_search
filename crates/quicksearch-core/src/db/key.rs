//! Process-global SQLCipher key, resolved once at startup before any
//! connection exists.

use std::sync::RwLock;

use super::schema::{HmacMode, Profile};
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

static PAGE_SIZE: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(super::schema::PAGE_SIZE);

/// Override the page size every subsequent open applies, for
/// `benches/page_geometry.rs` to sweep it. A process-global for the same
/// reason [`set_process_key`] is one: a keyed file's page size cannot be read
/// off the file — the header is ciphertext until SQLCipher has been told the
/// size — so it has to be known before the open, not derived during it.
///
/// **Measurement only.** Production never calls this, and an index seeded
/// under an override must be *opened* under the same one or it will not
/// decrypt.
#[doc(hidden)]
pub fn set_page_size_override(page_size: i64) {
    PAGE_SIZE.store(page_size, std::sync::atomic::Ordering::Relaxed);
}

/// `HmacMode` as an atom. The discriminants are private to this pair of
/// functions and never reach disk — the *reserve* is what the format records.
static HMAC_MODE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(encode_hmac(super::schema::HMAC_MODE));

const fn encode_hmac(mode: HmacMode) -> u8 {
    match mode {
        HmacMode::Off => 0,
        HmacMode::Sha256 => 1,
        HmacMode::Sha512 => 2,
    }
}

fn decode_hmac(byte: u8) -> HmacMode {
    match byte {
        0 => HmacMode::Off,
        1 => HmacMode::Sha256,
        _ => HmacMode::Sha512,
    }
}

/// Override the per-page authenticator every subsequent open applies, for
/// `benches/cipher_hmac.rs` to sweep it. A process-global for exactly the
/// reason [`set_page_size_override`] is one, and with the same warning: the
/// mode decides the page reserve, so a keyed file written under one and opened
/// under another decrypts to noise.
///
/// **Measurement only.** Production never calls this.
#[doc(hidden)]
pub fn set_hmac_mode_override(mode: HmacMode) {
    HMAC_MODE.store(encode_hmac(mode), std::sync::atomic::Ordering::Relaxed);
}

/// The layout this open should apply: [`super::schema::PROFILE`] unless a
/// measurement harness has overridden part of it.
pub(crate) fn current_profile() -> Profile {
    Profile {
        page_size: PAGE_SIZE.load(std::sync::atomic::Ordering::Relaxed),
        hmac: decode_hmac(HMAC_MODE.load(std::sync::atomic::Ordering::Relaxed)),
    }
}

/// `0` means "derive it from the index"; see [`set_search_cache_override`].
static SEARCH_CACHE_MIB: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Override the search connection's cache ceiling, in MiB, from
/// `[search] cache_size_mib`. `None` restores the derived value.
///
/// A process-global for the same reason the key is one: the search `Worker`
/// (`crate::search`) holds no `Config` — options travel per request in
/// `SearchOptions`, and this is a property of the *connection*, which the
/// worker opens and reopens on its own. Install it wherever the key is
/// installed, and again when settings are saved.
pub fn set_search_cache_override(cache_mib: Option<i64>) {
    SEARCH_CACHE_MIB.store(cache_mib.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
}

/// The configured override, or `None` to derive one.
pub(crate) fn search_cache_override() -> Option<i64> {
    match SEARCH_CACHE_MIB.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        mib => Some(mib),
    }
}
