//! Identity DB connection pool + migrations (step 10). SQLite via sqlx —
//! see CLAUDE.md's "Identity (step 10)" section for why this stack was
//! picked over alternatives.
//!
//! `sqlx::migrate!` embeds the SQL files under `migrations/` at compile
//! time (no DB connection needed to compile this) and runs them against
//! whatever DB `open()` connects to at startup — distinct from sqlx's
//! `query!`/`query_as!` macros (not used anywhere in this codebase; see
//! Cargo.toml's comment on the sqlx feature list for why).

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

/// Opens (creating if absent) the identity DB at `path` and runs any
/// pending migrations. Called once from `main()`, same lifecycle as
/// `auth::load_or_create_token`.
pub async fn open(path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
        .with_context(|| format!("parsing sqlite path {path}"))?
        .create_if_missing(true)
        // WAL mode: readers (e.g. a future admin-devices list query) don't
        // block on an in-flight write. Not load-bearing for correctness at
        // this DB's tiny write volume, but it's the standard default for
        // an sqlx+SQLite app and costs nothing here.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening identity DB at {path}"))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running identity DB migrations")?;

    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live smoke test (not just "does it compile") for the migration and
    /// the 2-admin-credential-cap trigger from 0001_identity.sql — a
    /// mistake in either would only surface later as a confusing runtime
    /// error, exactly the kind of thing this project's "test it, don't
    /// assume" convention exists to catch before it does.
    #[tokio::test]
    async fn migrations_run_and_admin_cap_trigger_enforces_two() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("identity_test.db");
        let pool = open(db_path.to_str().unwrap()).await.unwrap();

        let user_id = "test-user-1";
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, ?, ?, ?)")
            .bind(user_id)
            .bind("google-sub-1")
            .bind("test@example.com")
            .bind(0i64)
            .execute(&pool)
            .await
            .unwrap();

        let insert_cred = |cred_id: &'static str| {
            let pool = pool.clone();
            let user_id = user_id.to_string();
            async move {
                sqlx::query(
                    "INSERT INTO webauthn_credentials \
                     (id, user_id, credential_id, passkey_json, admin_tier, device_label, created_at) \
                     VALUES (?, ?, ?, ?, 1, ?, ?)",
                )
                .bind(cred_id)
                .bind(&user_id)
                .bind(cred_id.as_bytes())
                .bind("{}")
                .bind("test-device")
                .bind(0i64)
                .execute(&pool)
                .await
            }
        };

        insert_cred("cred-1").await.expect("1st admin credential should succeed");
        insert_cred("cred-2").await.expect("2nd admin credential should succeed");
        let third = insert_cred("cred-3").await;
        assert!(third.is_err(), "3rd admin-tier credential must be rejected by trg_webauthn_admin_cap");
        let err = third.unwrap_err().to_string();
        assert!(
            err.contains("admin-tier WebAuthn credential cap"),
            "expected the trigger's explicit RAISE message, got: {err}"
        );

        // Confirm only 2 rows actually landed — the failed 3rd insert
        // must not have partially committed anything.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webauthn_credentials WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }
}
