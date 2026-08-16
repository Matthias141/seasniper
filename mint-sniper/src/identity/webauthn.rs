//! WebAuthn passkey registration + login-time assertion (step 10e).
//!
//! The relying party id/origin are derived from `google_oauth_redirect_url`
//! rather than a separate config field — WebAuthn credentials are bound
//! to a specific origin, and this project's chosen origin (a Tailscale
//! MagicDNS HTTPS hostname, per 10c's redirect URL) is already the exact
//! thing Google Sign-In needs too. Reusing it means there is exactly ONE
//! place this codebase decides "this is our origin" rather than two that
//! could drift apart. Step 10.5's own explicit task text (10.5a) flags
//! that a LATER public hostname (Cloudflare Tunnel) would need its own
//! decision about whether it's the same rp_id or a second one — this is
//! the origin decision 10.5a inherits, not a new one it invents.
//!
//! Ceremony state (`PasskeyRegistration`/`PasskeyAuthentication`) is
//! held in-memory, keyed by session id — same "flow state doesn't belong
//! in the DB, it's ephemeral and mid-request" reasoning as
//! `identity::oidc::GoogleOidc::pending_flows`. At most one in-progress
//! ceremony per session at a time, which is the only shape a real
//! browser flow produces anyway.

use crate::identity::session;
use anyhow::{bail, Context, Result};
use sqlx::SqlitePool;
use std::collections::HashMap;
use tokio::sync::RwLock;
use webauthn_rs::prelude::*;

/// Hard cap from 10e's spec: at most 2 admin-tier credentials per user.
/// Enforced here (fail fast, clear error) AND at the DB level via
/// `trg_webauthn_admin_cap` in migration 0001 — defense in depth, same
/// convention as every other money-adjacent control in this codebase.
/// A 3rd registration attempt must fail clearly, never silently
/// overwrite the 1st/2nd or silently allow a 3rd.
const ADMIN_CREDENTIAL_CAP: i64 = 2;

pub struct WebauthnState {
    webauthn: Webauthn,
    pending_registrations: RwLock<HashMap<String, PasskeyRegistration>>,
    pending_authentications: RwLock<HashMap<String, PasskeyAuthentication>>,
}

impl WebauthnState {
    /// `redirect_url` is `Config::google_oauth_redirect_url` — see this
    /// module's doc comment for why. Fails loudly if that URL is missing
    /// a host (can't happen if `Config::validate` already ran, but this
    /// function doesn't assume that).
    pub fn new(redirect_url: &str, rp_name: &str) -> Result<Self> {
        let parsed = url::Url::parse(redirect_url).context("parsing google_oauth_redirect_url as WebAuthn rp_origin")?;
        let rp_id = parsed
            .host_str()
            .context("google_oauth_redirect_url has no host — cannot derive a WebAuthn rp_id from it")?
            .to_string();
        // rp_origin must be scheme+host+port with no path — WebauthnBuilder
        // rejects anything else. Build it fresh from the parsed components
        // rather than trying to string-trim the original URL.
        let mut origin = parsed.clone();
        origin.set_path("");
        origin.set_query(None);
        origin.set_fragment(None);

        let webauthn = WebauthnBuilder::new(&rp_id, &origin)
            .context("building WebauthnBuilder")?
            .rp_name(rp_name)
            .build()
            .context("building Webauthn")?;

        Ok(Self {
            webauthn,
            pending_registrations: RwLock::new(HashMap::new()),
            pending_authentications: RwLock::new(HashMap::new()),
        })
    }

    /// Starts a passkey registration ceremony for `user_id`. Enforces the
    /// admin-credential cap BEFORE even generating a challenge — a 3rd
    /// device shouldn't get as far as touching Face ID/Touch ID only to
    /// be rejected after the fact.
    pub async fn start_registration(
        &self,
        pool: &SqlitePool,
        session_id: &str,
        user_id: &str,
        email: &str,
    ) -> Result<CreationChallengeResponse> {
        let count = admin_credential_count(pool, user_id).await?;
        if count >= ADMIN_CREDENTIAL_CAP {
            bail!(
                "admin-tier device cap ({ADMIN_CREDENTIAL_CAP}) already reached for this account — \
                 revoke an existing device first (see /auth/webauthn/devices) if you need to register a new one"
            );
        }

        let user_uuid = uuid::Uuid::parse_str(user_id).context("user_id is not a valid UUID")?;
        let exclude = existing_credential_ids(pool, user_id).await?;

        let (ccr, reg_state) = self
            .webauthn
            .start_passkey_registration(user_uuid, email, email, Some(exclude))
            .map_err(|e| anyhow::anyhow!("starting WebAuthn registration: {e}"))?;

        self.pending_registrations.write().await.insert(session_id.to_string(), reg_state);
        Ok(ccr)
    }

    /// Completes registration: validates the browser's attestation
    /// response against the challenge from `start_registration`,
    /// re-checks the admin cap (a second registration could have raced
    /// in via a second tab between start and finish — the DB trigger is
    /// the final backstop either way), stores the credential, and marks
    /// this session's WebAuthn login stage done (proving you hold the
    /// device you just registered is exactly the same proof a login
    /// assertion would require).
    pub async fn finish_registration(
        &self,
        pool: &SqlitePool,
        session_id: &str,
        user_id: &str,
        device_label: &str,
        reg: &RegisterPublicKeyCredential,
    ) -> Result<()> {
        let reg_state = {
            let mut pending = self.pending_registrations.write().await;
            pending.remove(session_id)
        }
        .context("no in-progress registration for this session — call start_registration first")?;

        let passkey = self
            .webauthn
            .finish_passkey_registration(reg, &reg_state)
            .map_err(|e| anyhow::anyhow!("completing WebAuthn registration: {e}"))?;

        // Re-check the cap right before inserting — the DB trigger from
        // migration 0001 enforces this unconditionally regardless, but
        // checking here first gives a clear application-level error
        // message instead of a raw SQL constraint failure bubbling up.
        let count = admin_credential_count(pool, user_id).await?;
        if count >= ADMIN_CREDENTIAL_CAP {
            bail!("admin-tier device cap ({ADMIN_CREDENTIAL_CAP}) reached — registration rejected");
        }

        insert_credential(pool, user_id, &passkey, device_label).await?;
        session::mark_webauthn_verified(pool, session_id, passkey.cred_id()).await?;
        Ok(())
    }

    /// Starts a login-time authentication ceremony against ALL of
    /// `user_id`'s registered passkeys (never a global/discoverable
    /// lookup — this session already knows which user it belongs to
    /// from the Google sign-in step, so there's no "which account is
    /// this" ambiguity to resolve).
    pub async fn start_authentication(&self, pool: &SqlitePool, session_id: &str, user_id: &str) -> Result<RequestChallengeResponse> {
        let passkeys = load_passkeys(pool, user_id).await?;
        if passkeys.is_empty() {
            bail!("no WebAuthn devices registered for this account yet — register one first");
        }

        let (rcr, auth_state) = self
            .webauthn
            .start_passkey_authentication(&passkeys)
            .map_err(|e| anyhow::anyhow!("starting WebAuthn authentication: {e}"))?;

        self.pending_authentications.write().await.insert(session_id.to_string(), auth_state);
        Ok(rcr)
    }

    /// Completes a login-time authentication: validates the assertion,
    /// applies any counter/backup-state update the library says is
    /// needed (see `Passkey::update_credential`'s doc comment — most
    /// passkeys never need this, but when the counter DOES regress
    /// relative to what's stored, that's the library's clone-detection
    /// signal and this does not silently swallow it), bumps
    /// `last_used_at`, and marks this session's WebAuthn stage done.
    pub async fn finish_authentication(&self, pool: &SqlitePool, session_id: &str, reg: &PublicKeyCredential) -> Result<()> {
        let auth_state = {
            let mut pending = self.pending_authentications.write().await;
            pending.remove(session_id)
        }
        .context("no in-progress authentication for this session — call start_authentication first")?;

        let result = self
            .webauthn
            .finish_passkey_authentication(reg, &auth_state)
            .map_err(|e| anyhow::anyhow!("completing WebAuthn authentication: {e}"))?;

        let cred_id_bytes: &[u8] = result.cred_id();
        let Some((row_id, mut passkey)) = find_credential(pool, cred_id_bytes).await? else {
            bail!("authentication succeeded but the credential is no longer registered (was it revoked mid-ceremony?)");
        };

        if passkey.update_credential(&result) == Some(true) {
            update_credential_passkey(pool, &row_id, &passkey).await?;
        } else {
            touch_credential_last_used(pool, &row_id).await?;
        }

        session::mark_webauthn_verified(pool, session_id, cred_id_bytes).await?;
        Ok(())
    }
}

async fn admin_credential_count(pool: &SqlitePool, user_id: &str) -> Result<i64> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM webauthn_credentials WHERE user_id = ? AND admin_tier = 1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .context("counting admin-tier WebAuthn credentials")
}

async fn existing_credential_ids(pool: &SqlitePool, user_id: &str) -> Result<Vec<CredentialID>> {
    let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT credential_id FROM webauthn_credentials WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(pool)
        .await
        .context("loading existing credential IDs")?;
    Ok(rows.into_iter().map(CredentialID::from).collect())
}

async fn insert_credential(pool: &SqlitePool, user_id: &str, passkey: &Passkey, device_label: &str) -> Result<()> {
    let passkey_json = serde_json::to_string(passkey).context("serializing passkey")?;
    let cred_id_bytes: &[u8] = passkey.cred_id();
    sqlx::query(
        "INSERT INTO webauthn_credentials \
         (id, user_id, credential_id, passkey_json, admin_tier, device_label, created_at, last_used_at) \
         VALUES (?, ?, ?, ?, 1, ?, ?, NULL)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(cred_id_bytes)
    .bind(&passkey_json)
    .bind(device_label)
    .bind(crate::bus::now_ts() as i64)
    .execute(pool)
    .await
    .context("inserting WebAuthn credential (the DB-level admin-cap trigger may have rejected this)")?;
    Ok(())
}

async fn load_passkeys(pool: &SqlitePool, user_id: &str) -> Result<Vec<Passkey>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT passkey_json FROM webauthn_credentials WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(pool)
        .await
        .context("loading passkeys")?;
    rows.into_iter()
        .map(|json| serde_json::from_str::<Passkey>(&json).context("deserializing stored passkey"))
        .collect()
}

async fn find_credential(pool: &SqlitePool, cred_id_bytes: &[u8]) -> Result<Option<(String, Passkey)>> {
    let row = sqlx::query_as::<_, (String, String)>("SELECT id, passkey_json FROM webauthn_credentials WHERE credential_id = ?")
        .bind(cred_id_bytes)
        .fetch_optional(pool)
        .await
        .context("looking up credential by id")?;
    match row {
        Some((row_id, json)) => Ok(Some((row_id, serde_json::from_str(&json).context("deserializing stored passkey")?))),
        None => Ok(None),
    }
}

async fn update_credential_passkey(pool: &SqlitePool, row_id: &str, passkey: &Passkey) -> Result<()> {
    let passkey_json = serde_json::to_string(passkey).context("serializing updated passkey")?;
    sqlx::query("UPDATE webauthn_credentials SET passkey_json = ?, last_used_at = ? WHERE id = ?")
        .bind(&passkey_json)
        .bind(crate::bus::now_ts() as i64)
        .bind(row_id)
        .execute(pool)
        .await
        .context("persisting updated passkey")?;
    Ok(())
}

async fn touch_credential_last_used(pool: &SqlitePool, row_id: &str) -> Result<()> {
    sqlx::query("UPDATE webauthn_credentials SET last_used_at = ? WHERE id = ?")
        .bind(crate::bus::now_ts() as i64)
        .bind(row_id)
        .execute(pool)
        .await
        .context("bumping credential last_used_at")?;
    Ok(())
}

#[derive(serde::Serialize, sqlx::FromRow)]
pub struct DeviceRow {
    pub id: String,
    pub device_label: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// Lists a user's registered devices — the "view" half of 10e's "view
/// and revoke a registered device" requirement.
pub async fn list_devices(pool: &SqlitePool, user_id: &str) -> Result<Vec<DeviceRow>> {
    sqlx::query_as::<_, DeviceRow>(
        "SELECT id, device_label, created_at, last_used_at FROM webauthn_credentials WHERE user_id = ? ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .context("listing WebAuthn devices")
}

/// Revokes (hard-deletes) a device row, scoped to `user_id` so one
/// account can never revoke another's credential — a no-op today
/// (single user), but the right shape for step 11. Returns whether a row
/// was actually deleted, so the caller can 404 on a bad/foreign id
/// instead of silently reporting success.
pub async fn revoke_device(pool: &SqlitePool, user_id: &str, device_row_id: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM webauthn_credentials WHERE id = ? AND user_id = ?")
        .bind(device_row_id)
        .bind(user_id)
        .execute(pool)
        .await
        .context("revoking WebAuthn device")?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_pool() -> SqlitePool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webauthn_test.db");
        Box::leak(Box::new(dir));
        crate::db::open(path.to_str().unwrap()).await.unwrap()
    }

    // webauthn-rs deliberately exposes no public constructor for a bare
    // `Passkey` outside a real registration ceremony — `Passkey::cred`
    // stays `pub(crate)` even with `danger-credential-internals` enabled
    // (which only exposes the inner `Credential` type, not a way to
    // build a `Passkey` from one). A real ceremony needs a real browser/
    // platform authenticator, which this sandbox doesn't have — see this
    // step's report for what that means for what is and isn't verified
    // here. These DB-layer tests instead insert rows via the exact same
    // raw SQL `insert_credential` itself runs, with an arbitrary
    // `passkey_json` string standing in for a real serialized `Passkey`
    // — everything under test here (the admin cap, list/revoke, and
    // per-user scoping) operates on the row's `credential_id`/`user_id`/
    // `device_label` columns and never actually deserializes
    // `passkey_json`, so this is a faithful exercise of that logic
    // without needing a cryptographically real credential.
    async fn insert_fake_credential(pool: &SqlitePool, user_id: &str, cred_id: &[u8], device_label: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO webauthn_credentials \
             (id, user_id, credential_id, passkey_json, admin_tier, device_label, created_at, last_used_at) \
             VALUES (?, ?, ?, 'not-a-real-passkey', 1, ?, ?, NULL)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(user_id)
        .bind(cred_id)
        .bind(device_label)
        .bind(crate::bus::now_ts() as i64)
        .execute(pool)
        .await
        .context("inserting fake WebAuthn credential")?;
        Ok(())
    }

    #[tokio::test]
    async fn admin_cap_blocks_third_insert_at_db_level() {
        let pool = setup_pool().await;
        let user_id = "u1";
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, 'sub', 'e@x.com', 0)")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        insert_fake_credential(&pool, user_id, b"cred-a", "laptop").await.unwrap();
        insert_fake_credential(&pool, user_id, b"cred-b", "phone").await.unwrap();
        let third = insert_fake_credential(&pool, user_id, b"cred-c", "tablet").await;
        assert!(third.is_err(), "3rd credential must be rejected");

        assert_eq!(admin_credential_count(&pool, user_id).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn list_and_revoke_round_trip() {
        let pool = setup_pool().await;
        let user_id = "u2";
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, 'sub2', 'e2@x.com', 0)")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        insert_fake_credential(&pool, user_id, b"cred-x", "laptop").await.unwrap();

        let devices = list_devices(&pool, user_id).await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_label, "laptop");

        let revoked = revoke_device(&pool, user_id, &devices[0].id).await.unwrap();
        assert!(revoked);
        assert!(list_devices(&pool, user_id).await.unwrap().is_empty());

        // Revoking again (already gone) reports false, not an error or a
        // false "success".
        assert!(!revoke_device(&pool, user_id, &devices[0].id).await.unwrap());
    }

    #[tokio::test]
    async fn revoke_is_scoped_to_the_owning_user() {
        let pool = setup_pool().await;
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES ('u3', 's3', 'e3@x.com', 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES ('u4', 's4', 'e4@x.com', 0)")
            .execute(&pool)
            .await
            .unwrap();
        insert_fake_credential(&pool, "u3", b"cred-owned-by-u3", "laptop").await.unwrap();
        let devices = list_devices(&pool, "u3").await.unwrap();

        // u4 attempting to revoke u3's device must not succeed.
        let revoked = revoke_device(&pool, "u4", &devices[0].id).await.unwrap();
        assert!(!revoked);
        assert_eq!(list_devices(&pool, "u3").await.unwrap().len(), 1);
    }
}
