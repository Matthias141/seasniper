//! TOTP 2FA (step 10d). Algorithm/digits/step are set EXPLICITLY to
//! SHA1/6/30s even though totp-rs's `Builder::new()` already defaults to
//! exactly this (confirmed against the crate's real source, not assumed
//! — see 10a's report) — both for self-documentation and as a guard
//! against a future totp-rs major version silently changing its
//! defaults. This is what every mainstream authenticator app (Google
//! Authenticator, Authy, 1Password, etc.) expects; the crate's own docs
//! warn some apps silently fall back to SHA1 from SHA256/SHA512 anyway,
//! which is one more reason to just pin SHA1 outright rather than rely
//! on an app "supporting" something else.
//!
//! Secrets are stored encrypted at rest (`identity::crypto`) and are
//! never treated as "2FA enabled" (`totp_secrets.enabled`) until one
//! real `check()` round-trip has proven the secret actually works — see
//! `verify_setup`.
//!
//! Replay protection: totp-rs's own docs are explicit that `check` does
//! NOT enforce single-use of a code — see migration 0003's doc comment.
//! `last_used_step` closes that gap here.

use crate::identity::crypto;
use aes_gcm::Aes256Gcm;
use anyhow::{bail, Context, Result};
use sqlx::SqlitePool;
use totp_rs::{Algorithm, Builder, Secret, Totp};

const ISSUER: &str = "mint-sniper";

fn build_totp(secret_bytes: Vec<u8>, account_name: &str) -> Result<Totp> {
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_step_duration(30)
        .with_secret(secret_bytes)
        .with_issuer(Some(ISSUER))
        .with_account_name(account_name)
        .build()
        .map_err(|e| anyhow::anyhow!("building TOTP: {e:?}"))
}

pub struct SetupMaterial {
    /// `data:image/png;base64,...` — safe to drop straight into an <img
    /// src>. Never persisted; regenerable from the stored secret at any
    /// time (though there's no route to do so as of 10d — a lost QR code
    /// before setup is completed just means starting setup over).
    pub qr_data_uri: String,
    /// Base32 form of the same secret, for manual entry when a camera
    /// isn't available. Same "never persisted beyond this one response"
    /// treatment as the QR code.
    pub secret_base32: String,
}

/// Generates a fresh secret, stores it encrypted (REPLACING any existing
/// row for this user — see this function's note on why that's allowed),
/// and returns the material needed to enroll it in an authenticator app.
/// `enabled` starts at 0: nothing about this call makes the secret live
/// until `verify_setup` proves it.
///
/// Replacing an existing row on re-run is a deliberate, single-user-scope
/// decision: this session already passed Google sign-in to reach this
/// endpoint (see api.rs's require_session), and there's no other
/// self-service "I lost my authenticator, let me redo TOTP setup" path
/// built yet. Step 11 (multi-user) is where this would need tightening
/// (e.g. requiring step-up auth from the OLD factor before replacing it)
/// — noted, not built, per this step's explicit scope boundary.
pub async fn start_setup(pool: &SqlitePool, cipher: &Aes256Gcm, user_id: &str, account_name: &str) -> Result<SetupMaterial> {
    let secret = Secret::generate();
    let secret_base32 = secret.to_base32();
    let secret_bytes = secret.as_bytes().to_vec();

    let totp = build_totp(secret_bytes.clone(), account_name)?;
    // to_qr_base64() returns the RAW base64 PNG, with no data: URI prefix
    // — confirmed against totp-rs's actual source after a first version
    // of this test asserted the prefix was already there and failed.
    // Building the prefix here, once, rather than expecting every caller
    // (today: one route in api.rs) to remember to add it.
    let qr_base64 = totp.to_qr_base64().map_err(|e| anyhow::anyhow!("generating QR code: {e:?}"))?;
    let qr_data_uri = format!("data:image/png;base64,{qr_base64}");

    let (ciphertext, nonce) = crypto::encrypt_totp_secret(cipher, &secret_bytes)?;

    sqlx::query(
        "INSERT INTO totp_secrets (user_id, secret_ciphertext, secret_nonce, enabled, created_at, last_used_step) \
         VALUES (?, ?, ?, 0, ?, NULL) \
         ON CONFLICT(user_id) DO UPDATE SET \
             secret_ciphertext = excluded.secret_ciphertext, \
             secret_nonce = excluded.secret_nonce, \
             enabled = 0, \
             created_at = excluded.created_at, \
             last_used_step = NULL",
    )
    .bind(user_id)
    .bind(&ciphertext)
    .bind(&nonce)
    .bind(crate::bus::now_ts() as i64)
    .execute(pool)
    .await
    .context("storing TOTP secret")?;

    Ok(SetupMaterial { qr_data_uri, secret_base32 })
}

struct StoredSecret {
    totp: Totp,
    enabled: bool,
    last_used_step: Option<i64>,
}

async fn load(pool: &SqlitePool, cipher: &Aes256Gcm, user_id: &str, account_name: &str) -> Result<Option<StoredSecret>> {
    let row = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, bool, Option<i64>)>(
        "SELECT secret_ciphertext, secret_nonce, enabled, last_used_step FROM totp_secrets WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .context("loading TOTP secret")?;

    let Some((ciphertext, nonce, enabled, last_used_step)) = row else {
        return Ok(None);
    };
    let secret_bytes = crypto::decrypt_totp_secret(cipher, &ciphertext, &nonce)?;
    let totp = build_totp(secret_bytes, account_name)?;
    Ok(Some(StoredSecret { totp, enabled, last_used_step }))
}

/// Completes setup: proves the operator's authenticator app actually has
/// a working copy of the secret just generated by `start_setup`. On
/// success, flips `enabled = 1` and records the accepted step (replay
/// protection baseline). Deliberately does NOT require `enabled` to
/// already be true — that's the whole point of a setup-verification step.
pub async fn verify_setup(pool: &SqlitePool, cipher: &Aes256Gcm, user_id: &str, account_name: &str, code: &str) -> Result<bool> {
    let Some(stored) = load(pool, cipher, user_id, account_name).await? else {
        bail!("no TOTP secret has been generated for this user yet — call start_setup first");
    };

    let now = crate::bus::now_ts();
    let Some(step) = stored.totp.check(code, now) else {
        return Ok(false);
    };

    sqlx::query("UPDATE totp_secrets SET enabled = 1, last_used_step = ? WHERE user_id = ?")
        .bind(step as i64)
        .bind(user_id)
        .execute(pool)
        .await
        .context("enabling TOTP after successful setup verification")?;
    Ok(true)
}

/// Verifies a code for an ALREADY-enabled secret — the per-login check
/// (see identity/session.rs's doc comment on the Google -> TOTP ->
/// WebAuthn chain). Enforces replay protection: a code is only accepted
/// if its matched step is strictly greater than the last one accepted
/// for this user, across any session — see migration 0003.
pub async fn verify_login(pool: &SqlitePool, cipher: &Aes256Gcm, user_id: &str, account_name: &str, code: &str) -> Result<bool> {
    let Some(stored) = load(pool, cipher, user_id, account_name).await? else {
        return Ok(false);
    };
    if !stored.enabled {
        return Ok(false);
    }

    let now = crate::bus::now_ts();
    let Some(step) = stored.totp.check(code, now) else {
        return Ok(false);
    };
    if let Some(last) = stored.last_used_step {
        if (step as i64) <= last {
            return Ok(false);
        }
    }

    sqlx::query("UPDATE totp_secrets SET last_used_step = ? WHERE user_id = ?")
        .bind(step as i64)
        .bind(user_id)
        .execute(pool)
        .await
        .context("recording accepted TOTP step")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::crypto as id_crypto;

    async fn setup_pool() -> SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("totp_test.db");
        // Leak the tempdir so it outlives the returned pool for the
        // duration of the test — fine for a short-lived test process.
        Box::leak(Box::new(dir));
        crate::db::open(path.to_str().unwrap()).await.unwrap()
    }

    fn test_keys() -> id_crypto::IdentityKeys {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".session-key");
        let keys = id_crypto::load_or_create(path.to_str().unwrap()).unwrap();
        Box::leak(Box::new(dir));
        keys
    }

    #[tokio::test]
    async fn full_setup_and_login_round_trip_with_replay_protection() {
        let pool = setup_pool().await;
        let keys = test_keys();
        let user_id = "user-1";
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, 'sub', 'a@b.com', 0)")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        let material = start_setup(&pool, &keys.totp_cipher, user_id, "a@b.com").await.unwrap();
        assert!(material.qr_data_uri.starts_with("data:image/png;base64,"));
        assert!(!material.secret_base32.is_empty());

        // Confirm the payload is a real, well-formed PNG (the 8-byte PNG
        // magic number), not just a string that happens to start with
        // the right prefix. This project's Rust toolchain has no QR
        // decoder available to independently re-scan the code inline, so
        // the actual round-trip check — a real ZXing decoder reading this
        // exact PNG back to the exact otpauth:// URL — was done manually
        // outside this test suite (Python + the zxing-cpp package) and is
        // recorded in this step's commit message, not silently assumed.
        let b64 = material.qr_data_uri.strip_prefix("data:image/png;base64,").unwrap();
        let png_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        assert_eq!(&png_bytes[0..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A], "not a valid PNG header");

        // Reconstruct the real TOTP from the base32 shown to the
        // "operator" to generate a genuinely valid current code — this
        // is what an authenticator app would do, not a shortcut around
        // the actual crypto.
        let secret = Secret::try_from_base32(&material.secret_base32).unwrap();
        let totp = build_totp(secret.as_bytes().to_vec(), "a@b.com").unwrap();
        let code = totp.generate_current().to_string();

        // Wrong code is rejected, doesn't enable anything.
        assert!(!verify_setup(&pool, &keys.totp_cipher, user_id, "a@b.com", "000000").await.unwrap());
        // Real code completes setup.
        assert!(verify_setup(&pool, &keys.totp_cipher, user_id, "a@b.com", &code).await.unwrap());

        // The SAME code must not be usable again (replay protection) —
        // this is the exact gap totp-rs's own docs call out as the
        // caller's responsibility.
        assert!(!verify_login(&pool, &keys.totp_cipher, user_id, "a@b.com", &code).await.unwrap());

        // A fresh code (next step) DOES work for a subsequent login.
        let next_code = totp.generate(crate::bus::now_ts() + 30).to_string();
        assert!(verify_login(&pool, &keys.totp_cipher, user_id, "a@b.com", &next_code).await.unwrap());
    }

    #[tokio::test]
    async fn verify_login_rejects_before_setup_is_completed() {
        let pool = setup_pool().await;
        let keys = test_keys();
        let user_id = "user-2";
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, 'sub2', 'c@d.com', 0)")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        let material = start_setup(&pool, &keys.totp_cipher, user_id, "c@d.com").await.unwrap();
        let secret = Secret::try_from_base32(&material.secret_base32).unwrap();
        let totp = build_totp(secret.as_bytes().to_vec(), "c@d.com").unwrap();
        let code = totp.generate_current().to_string();

        // enabled is still 0 — a correct code must still be rejected by
        // the LOGIN path (as opposed to the setup path) until setup
        // verification has actually run.
        assert!(!verify_login(&pool, &keys.totp_cipher, user_id, "c@d.com", &code).await.unwrap());
    }
}

