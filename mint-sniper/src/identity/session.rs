//! DB-backed session model (step 10c onward). A session row is created
//! the moment Google sign-in succeeds and is upgraded in place as TOTP
//! (10d) and a WebAuthn assertion (10e) each complete — see
//! `migrations/0002_session_login_stages.sql`'s doc comment. `admin_tier`
//! is 1 only once all three stages are done; it is NOT the same concept
//! as step-up auth (10f), which is a separate, per-request freshness
//! check layered on top of an admin_tier session.

use anyhow::{Context, Result};
use rand::RngCore;
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub admin_tier: bool,
    pub device_label: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub revoked_at: Option<i64>,
    pub totp_verified_at: Option<i64>,
    pub webauthn_verified_at: Option<i64>,
}

/// Generates a fresh opaque session id. 32 random bytes, hex-encoded —
/// same entropy/format convention as `auth::load_or_create_token`'s API
/// token. This is the only value that ever goes into the encrypted
/// cookie; everything else about the session lives server-side in this
/// table, which is what makes 10e's device-revoke actually work.
fn new_session_id() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    alloy::hex::encode(bytes)
}

fn new_row_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Finds the user with this Google `sub`, or creates one. There is
/// deliberately no cap or invite-list check here yet — step 10 is built
/// for exactly one identity, so the very first Google account to
/// complete this flow becomes "the" user. This is safe only because the
/// OAuth client itself (10c's config) is a credential the operator
/// controls and because nothing downstream trusts a session until it
/// reaches admin_tier via TOTP + WebAuthn too. Step 11 (multi-user,
/// invites) is exactly where an allow-list belongs; deliberately not
/// built here per this step's scope boundary.
pub async fn find_or_create_user(pool: &SqlitePool, google_sub: &str, email: &str) -> Result<String> {
    if let Some(id) = sqlx::query_scalar::<_, String>("SELECT id FROM users WHERE google_sub = ?")
        .bind(google_sub)
        .fetch_optional(pool)
        .await
        .context("looking up user by google_sub")?
    {
        // Email can change on the Google side (see users.google_sub's own
        // doc comment in the migration) — keep it in sync on every
        // sign-in rather than only at creation, since it's audit/display
        // only and never used as a lookup key.
        sqlx::query("UPDATE users SET email = ? WHERE id = ?")
            .bind(email)
            .bind(&id)
            .execute(pool)
            .await
            .context("syncing user email")?;
        return Ok(id);
    }

    let id = new_row_id();
    sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(google_sub)
        .bind(email)
        .bind(crate::bus::now_ts() as i64)
        .execute(pool)
        .await
        .context("inserting new user")?;
    Ok(id)
}

/// Creates a new session row at the "Google done, nothing else yet"
/// stage. Returns the opaque session id to put in the cookie.
pub async fn create_google_verified_session(pool: &SqlitePool, user_id: &str) -> Result<String> {
    let id = new_session_id();
    let now = crate::bus::now_ts() as i64;
    sqlx::query(
        "INSERT INTO sessions (id, user_id, admin_tier, created_at, last_seen_at, totp_verified_at, webauthn_verified_at) \
         VALUES (?, ?, 0, ?, ?, NULL, NULL)",
    )
    .bind(&id)
    .bind(user_id)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .context("inserting new session")?;
    Ok(id)
}

/// Looks up a session by id. Returns `None` for a missing OR revoked
/// session — callers should treat both identically (an absent/invalid
/// session), never distinguish "revoked" from "never existed" in an
/// error message, since that would leak whether a given session id was
/// ever real to anyone who happened to have it.
pub async fn get_active(pool: &SqlitePool, session_id: &str) -> Result<Option<Session>> {
    let row = sqlx::query_as::<_, Session>(
        "SELECT id, user_id, admin_tier, device_label, created_at, last_seen_at, revoked_at, \
                totp_verified_at, webauthn_verified_at \
         FROM sessions WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .context("looking up session")?;
    Ok(row)
}

pub async fn touch(pool: &SqlitePool, session_id: &str) -> Result<()> {
    sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
        .bind(crate::bus::now_ts() as i64)
        .bind(session_id)
        .execute(pool)
        .await
        .context("touching session last_seen_at")?;
    Ok(())
}

/// Marks the TOTP stage done for this session and promotes to
/// `admin_tier` if the WebAuthn stage is already done too (order between
/// TOTP and WebAuthn isn't fixed — whichever completes second is what
/// flips admin_tier).
pub async fn mark_totp_verified(pool: &SqlitePool, session_id: &str) -> Result<()> {
    let now = crate::bus::now_ts() as i64;
    sqlx::query("UPDATE sessions SET totp_verified_at = ? WHERE id = ?")
        .bind(now)
        .bind(session_id)
        .execute(pool)
        .await
        .context("marking totp_verified_at")?;
    promote_if_complete(pool, session_id).await
}

pub async fn mark_webauthn_verified(pool: &SqlitePool, session_id: &str, credential_id: &[u8]) -> Result<()> {
    let now = crate::bus::now_ts() as i64;
    sqlx::query("UPDATE sessions SET webauthn_verified_at = ?, webauthn_credential_id = ? WHERE id = ?")
        .bind(now)
        .bind(credential_id)
        .bind(session_id)
        .execute(pool)
        .await
        .context("marking webauthn_verified_at")?;
    promote_if_complete(pool, session_id).await
}

async fn promote_if_complete(pool: &SqlitePool, session_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE sessions SET admin_tier = 1 \
         WHERE id = ? AND totp_verified_at IS NOT NULL AND webauthn_verified_at IS NOT NULL",
    )
    .bind(session_id)
    .execute(pool)
    .await
    .context("promoting session to admin_tier")?;
    Ok(())
}

/// Looks up a user's email by id — used wherever TOTP/WebAuthn need an
/// account_name/display value (see totp.rs's `account_name` parameter).
/// Not cached: this DB is tiny and local, and email can change (see
/// `find_or_create_user`'s doc comment), so always reading it fresh here
/// is simpler than inventing an invalidation story for a cache.
pub async fn get_user_email(pool: &SqlitePool, user_id: &str) -> Result<String> {
    sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .context("looking up user email")
}

/// Soft-revokes a session (10e's device revoke, and anywhere else a
/// session needs to be killed server-side). Sets `revoked_at` rather than
/// deleting the row, so a revoked device still shows up in a device
/// list with a visible "revoked" state instead of silently vanishing.
pub async fn revoke(pool: &SqlitePool, session_id: &str) -> Result<()> {
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(crate::bus::now_ts() as i64)
        .bind(session_id)
        .execute(pool)
        .await
        .context("revoking session")?;
    Ok(())
}
