//! Process-global SQLCipher key, resolved once at startup before any
//! connection exists.

use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
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

/// Hex form of the installed key, if any, for the GUI's keychain "remember"
/// toggle, which stores the derived key and never the password.
pub fn process_key_hex() -> Option<String> {
    PROCESS_KEY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|k| k.to_hex())
}

static PAGE_SIZE: AtomicI64 = AtomicI64::new(super::schema::PAGE_SIZE);

/// Override the page size of every later open, for
/// `benches/page_geometry.rs`. Process-global because a keyed file's header
/// is ciphertext, so the page size must be known before opening.
///
/// **Measurement only:** an index built under an override must be opened
/// under the same one.
#[doc(hidden)]
pub fn set_page_size_override(page_size: i64) {
    PAGE_SIZE.store(page_size, Ordering::Relaxed);
}

/// [`HmacMode`] as an atom; the value never reaches disk.
static HMAC_MODE: AtomicU8 = AtomicU8::new(super::schema::HMAC_MODE as u8);

fn decode_hmac(byte: u8) -> HmacMode {
    match byte {
        0 => HmacMode::Off,
        1 => HmacMode::Sha256,
        _ => HmacMode::Sha512,
    }
}

/// Override the per-page authenticator of every later open, for
/// `benches/cipher_hmac.rs`. Process-global like [`set_page_size_override`]:
/// the mode sets the page reserve, so a keyed file written under one mode
/// does not decrypt under another.
///
/// **Measurement only.**
#[doc(hidden)]
pub fn set_hmac_mode_override(mode: HmacMode) {
    HMAC_MODE.store(mode as u8, Ordering::Relaxed);
}

/// The layout this open should apply: [`super::schema::PROFILE`] unless a
/// measurement harness has overridden part of it.
pub(crate) fn current_profile() -> Profile {
    Profile {
        page_size: PAGE_SIZE.load(Ordering::Relaxed),
        hmac: decode_hmac(HMAC_MODE.load(Ordering::Relaxed)),
    }
}

/// `0` means "derive it from the index"; see [`set_search_cache_override`].
static SEARCH_CACHE_MIB: AtomicI64 = AtomicI64::new(0);

/// Override the search connection's cache ceiling in MiB
/// (`[search] cache_size_mib`); `None` restores the derived value.
/// Process-global because the search worker opens its own connections and
/// holds no `Config`. Set it wherever the key is set, and again when settings
/// are saved.
pub fn set_search_cache_override(cache_mib: Option<i64>) {
    SEARCH_CACHE_MIB.store(cache_mib.unwrap_or(0), Ordering::Relaxed);
}

/// The configured override, or `None` to derive one.
pub(crate) fn search_cache_override() -> Option<i64> {
    match SEARCH_CACHE_MIB.load(Ordering::Relaxed) {
        0 => None,
        mib => Some(mib),
    }
}
