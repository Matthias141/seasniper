//! Key material for step 10: the axum-extra private-cookie-jar signing/
//! encryption key, and the AES-256-GCM key TOTP secrets are encrypted
//! with at rest (see totp_secrets.secret_ciphertext in the migration).
//!
//! Same on-first-run-generate-and-persist pattern as
//! `auth::load_or_create_token` — one file (`.session-key`), gitignored,
//! 0600 on unix. 96 random bytes: the first 64 are axum-extra's
//! `cookie::Key` (it requires exactly 64 bytes — 32 for signing, 32 for
//! encryption), the remaining 32 are the AES-256-GCM key. One file rather
//! than two: both are "the secret that, if it leaked, would let someone
//! forge a session or decrypt a TOTP secret" — the same blast radius
//! class, so splitting them into separate files would just be two files
//! to protect identically instead of one.

use aes_gcm::{Aes256Gcm, Key, KeyInit};
use anyhow::{bail, Context, Result};
use axum_extra::extract::cookie::Key as CookieKey;
use rand::RngCore;
use std::fs;

const KEY_MATERIAL_LEN: usize = 96;

pub struct IdentityKeys {
    pub cookie_key: CookieKey,
    pub totp_cipher: Aes256Gcm,
}

pub fn load_or_create(path: &str) -> Result<IdentityKeys> {
    let bytes = if let Ok(existing) = fs::read_to_string(path) {
        let trimmed = existing.trim();
        let decoded = alloy::hex::decode(trimmed).context("decoding .session-key contents as hex")?;
        if decoded.len() != KEY_MATERIAL_LEN {
            bail!(
                "{path} has {} bytes of key material, expected {KEY_MATERIAL_LEN} — \
                 delete it to regenerate (this invalidates all existing sessions and \
                 re-encrypts nothing automatically, so any enrolled TOTP secret would \
                 also need to be re-set-up)",
                decoded.len()
            );
        }
        decoded
    } else {
        let mut bytes = vec![0u8; KEY_MATERIAL_LEN];
        rand::thread_rng().fill_bytes(&mut bytes);
        fs::write(path, alloy::hex::encode(&bytes)).with_context(|| format!("writing key material to {path}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        bytes
    };

    let cookie_key = CookieKey::from(&bytes[0..64]);
    // `Key::<Aes256Gcm>::from_slice` is deprecated in aes-gcm 0.10's
    // generic-array 0.14 (it wants generic-array 1.x, which is aes-gcm
    // 0.11's incompatible API — see Cargo.toml's comment on why 0.10 was
    // pinned instead). `#[allow(deprecated)]` here rather than switching:
    // no functional difference, this is the one call site.
    #[allow(deprecated)]
    let totp_key = Key::<Aes256Gcm>::from_slice(&bytes[64..96]);
    let totp_cipher = Aes256Gcm::new(totp_key);

    Ok(IdentityKeys { cookie_key, totp_cipher })
}

/// Encrypts `plaintext` (a TOTP secret's raw bytes) with a fresh random
/// nonce, returning `(ciphertext, nonce)` for storage in
/// `totp_secrets.secret_ciphertext` / `secret_nonce`. A fresh nonce per
/// encryption (never reused) is what makes AES-GCM safe here — this
/// column is only ever written once per user in practice (10d's setup
/// flow), but the API doesn't rely on that to stay safe.
pub fn encrypt_totp_secret(cipher: &Aes256Gcm, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    use aes_gcm::aead::{Aead, OsRng};
    use aes_gcm::AeadCore;

    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("encrypting TOTP secret: {e}"))?;
    Ok((ciphertext, nonce.to_vec()))
}

pub fn decrypt_totp_secret(cipher: &Aes256Gcm, ciphertext: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
    use aes_gcm::aead::Aead;
    use aes_gcm::Nonce;

    if nonce.len() != 12 {
        bail!("stored TOTP nonce has {} bytes, expected 12", nonce.len());
    }
    #[allow(deprecated)] // see the matching note in load_or_create above
    let nonce = Nonce::from_slice(nonce);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("decrypting TOTP secret (wrong key, or corrupted row): {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let mut bytes = vec![0u8; KEY_MATERIAL_LEN];
        rand::thread_rng().fill_bytes(&mut bytes);
        let cipher = Aes256Gcm::new_from_slice(&bytes[64..96]).unwrap();

        let secret = b"JBSWY3DPEHPK3PXP";
        let (ciphertext, nonce) = encrypt_totp_secret(&cipher, secret).unwrap();
        assert_ne!(ciphertext, secret, "ciphertext must not equal plaintext");

        let decrypted = decrypt_totp_secret(&cipher, &ciphertext, &nonce).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn decrypt_fails_with_wrong_key() {
        let mut bytes_a = vec![0u8; KEY_MATERIAL_LEN];
        rand::thread_rng().fill_bytes(&mut bytes_a);
        let cipher_a = Aes256Gcm::new_from_slice(&bytes_a[64..96]).unwrap();

        let mut bytes_b = vec![0u8; KEY_MATERIAL_LEN];
        rand::thread_rng().fill_bytes(&mut bytes_b);
        let cipher_b = Aes256Gcm::new_from_slice(&bytes_b[64..96]).unwrap();

        let (ciphertext, nonce) = encrypt_totp_secret(&cipher_a, b"secret").unwrap();
        assert!(decrypt_totp_secret(&cipher_b, &ciphertext, &nonce).is_err());
    }

    #[test]
    fn load_or_create_persists_and_reloads_identically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".session-key");
        let path_str = path.to_str().unwrap();

        let first = load_or_create(path_str).unwrap();
        let second = load_or_create(path_str).unwrap();

        // Same underlying key material both times (round-trips through
        // the file correctly) — proven indirectly by encrypting with one
        // instance and decrypting with the other.
        let (ciphertext, nonce) = encrypt_totp_secret(&first.totp_cipher, b"round-trip-check").unwrap();
        let decrypted = decrypt_totp_secret(&second.totp_cipher, &ciphertext, &nonce).unwrap();
        assert_eq!(decrypted, b"round-trip-check");
    }
}
