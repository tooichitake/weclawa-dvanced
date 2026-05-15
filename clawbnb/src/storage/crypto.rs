//! Token-at-rest encryption (Phase 6.1).
//!
//! Every iLink bot token written to `accounts` is encrypted under
//! AES-256-GCM with a per-row random nonce. The 32-byte master key is
//! resolved at process start in this order:
//!
//! 1. `WECLAWBOT_DB_KEY` env var (base64-encoded, 32 raw bytes
//!    expected). Used by operators who already have a secret-management
//!    pipeline (Vault, AWS Secrets Manager, sealed-secret on k8s).
//! 2. `~/.weclawbot/.db-key` (mode 0600). Auto-created on first boot
//!    with 32 random bytes from the OS CSPRNG. Backed up by the
//!    operator like any other unrecoverable credential.
//!
//! The key is cached in a `OnceLock` for the lifetime of the process —
//! we don't re-read the file for every encrypt call. Rotating the key
//! is a planned Phase 6.5: re-encrypt every row under a new master.
//!
//! ### Why AES-256-GCM and not (say) chacha20-poly1305
//!
//! Both are fine; AES has hardware acceleration on every x86-64 server
//! built in the last decade (AES-NI) and ARMv8 (Crypto Extensions). For
//! a self-hosted Rust daemon shipping to Linux/macOS the AES path is
//! ~3-5× faster at the hot spots than the SW chacha implementation.

use std::sync::OnceLock;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use rand::TryRngCore;

const KEY_LEN: usize = 32; // AES-256
const NONCE_LEN: usize = 12; // 96-bit GCM nonce

static MASTER_KEY: OnceLock<[u8; KEY_LEN]> = OnceLock::new();

/// Load the master key and cache it. Idempotent — second call returns
/// the cached value without touching env or disk.
pub fn ensure_master_key() -> Result<&'static [u8; KEY_LEN], String> {
    if let Some(k) = MASTER_KEY.get() {
        return Ok(k);
    }
    let key = resolve_key()?;
    let _ = MASTER_KEY.set(key); // someone else may have raced — ignore
    Ok(MASTER_KEY.get().expect("just set"))
}

fn resolve_key() -> Result<[u8; KEY_LEN], String> {
    if let Ok(b64) = std::env::var("WECLAWBOT_DB_KEY") {
        if !b64.is_empty() {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|e| format!("WECLAWBOT_DB_KEY base64 decode: {e}"))?;
            if raw.len() != KEY_LEN {
                return Err(format!(
                    "WECLAWBOT_DB_KEY: expected {KEY_LEN}-byte key, got {}",
                    raw.len()
                ));
            }
            let mut out = [0u8; KEY_LEN];
            out.copy_from_slice(&raw);
            return Ok(out);
        }
    }
    load_or_create_key_file()
}

fn load_or_create_key_file() -> Result<[u8; KEY_LEN], String> {
    let path = crate::storage::state_dir::state_dir().join(".db-key");
    if path.exists() {
        let raw = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if raw.len() != KEY_LEN {
            return Err(format!(
                "{} is corrupt: expected {KEY_LEN} bytes, got {}",
                path.display(),
                raw.len()
            ));
        }
        let mut out = [0u8; KEY_LEN];
        out.copy_from_slice(&raw);
        return Ok(out);
    }
    // First-time generation.
    let mut key = [0u8; KEY_LEN];
    rand::rngs::OsRng
        .try_fill_bytes(&mut key)
        .map_err(|e| format!("rng: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, key).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    tracing::warn!(
        "auto-generated DB encryption key at {}. BACK THIS FILE UP — \
         losing it makes encrypted tokens unrecoverable.",
        path.display()
    );
    Ok(key)
}

/// Encrypt `plaintext` under the master key. Returns `(ciphertext, nonce)`.
/// Each call uses a fresh random nonce — never reuse.
pub fn encrypt(plaintext: &[u8]) -> Result<(Vec<u8>, [u8; NONCE_LEN]), String> {
    let key_bytes = ensure_master_key()?;
    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|e| format!("rng: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| format!("aes-gcm encrypt: {e}"))?;
    Ok((ct, nonce_bytes))
}

/// Derive a fixed-length subkey from the master key for a specific
/// purpose. Different `label` strings produce different keys, so the
/// SSO-session HMAC key, the (future) backup-encryption key, etc. are
/// cryptographically separated from the AES-GCM token-at-rest key.
///
/// Implementation: HMAC-SHA256(master, label) → 32 bytes. This is the
/// HKDF "expand" step with a fixed length output, no salt. Plenty for
/// short-lived purposes like cookie signing where we just need
/// "different label = different bytes, attacker can't compute from
/// either key to the other".
///
/// `label` should be a stable string with a version tag, e.g.
/// `"sso-session-v1"`. If the cookie format ever changes incompatibly,
/// bump to `"sso-session-v2"` to invalidate all live cookies.
pub fn derive_subkey(label: &str) -> Result<[u8; KEY_LEN], String> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let master = ensure_master_key()?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(master)
        .map_err(|e| format!("hmac init: {e}"))?;
    mac.update(label.as_bytes());
    let digest = mac.finalize().into_bytes();
    // Sha256 output is exactly 32 bytes which is exactly KEY_LEN.
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&digest);
    Ok(out)
}

/// Decrypt `(ciphertext, nonce)` under the master key.
pub fn decrypt(ciphertext: &[u8], nonce_bytes: &[u8]) -> Result<Vec<u8>, String> {
    if nonce_bytes.len() != NONCE_LEN {
        return Err(format!(
            "nonce length: expected {NONCE_LEN}, got {}",
            nonce_bytes.len()
        ));
    }
    let key_bytes = ensure_master_key()?;
    let key = Key::<Aes256Gcm>::from_slice(key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("aes-gcm decrypt (auth fail or wrong key): {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reset the OnceLock for tests by writing a known key into a temp
    /// state dir. Note: the OnceLock is per-process; once set in one
    /// test, subsequent tests in the same binary share that key.
    fn ensure_test_key() {
        if MASTER_KEY.get().is_none() {
            // SAFETY: env mutation in tests — the runner serializes
            // tests within a module so concurrent reads aren't a worry
            // here. We use a deterministic 32-byte key.
            unsafe {
                std::env::set_var(
                    "WECLAWBOT_DB_KEY",
                    base64::engine::general_purpose::STANDARD.encode([0xAB; KEY_LEN]),
                );
            }
            let _ = ensure_master_key();
        }
    }

    #[test]
    fn round_trip_basic() {
        ensure_test_key();
        let (ct, nonce) = encrypt(b"hello-token").unwrap();
        let pt = decrypt(&ct, &nonce).unwrap();
        assert_eq!(pt, b"hello-token");
    }

    #[test]
    fn distinct_nonces_for_each_call() {
        ensure_test_key();
        let (_ct1, n1) = encrypt(b"x").unwrap();
        let (_ct2, n2) = encrypt(b"x").unwrap();
        assert_ne!(n1, n2, "AES-GCM MUST never reuse a (key, nonce) pair");
    }

    #[test]
    fn distinct_ciphertexts_for_repeated_plaintext() {
        ensure_test_key();
        let (ct1, _) = encrypt(b"same-input").unwrap();
        let (ct2, _) = encrypt(b"same-input").unwrap();
        // Because nonces differ, ciphertexts differ — observable
        // encryption (no IND-CPA leak via ciphertext equality).
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn auth_failure_on_tampered_ciphertext() {
        ensure_test_key();
        let (mut ct, nonce) = encrypt(b"important").unwrap();
        ct[0] ^= 0xFF; // flip a bit
        let err = decrypt(&ct, &nonce).unwrap_err();
        assert!(err.contains("auth fail"));
    }

    #[test]
    fn wrong_nonce_length_rejected() {
        ensure_test_key();
        let (ct, _) = encrypt(b"x").unwrap();
        assert!(decrypt(&ct, &[0u8; 11]).is_err());
        assert!(decrypt(&ct, &[0u8; 13]).is_err());
    }
}
