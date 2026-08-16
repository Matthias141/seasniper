//! Local bearer-token auth for the control API, and (step 10g) the
//! decision about how it coexists with step 10's per-identity session
//! auth once that's configured.
//!
//! Binding to 127.0.0.1 stops anything off-machine, but not everything on
//! it: with a permissive CORS policy (the old `allow_origin(Any)`), any
//! webpage open in the same browser — a malicious tab, an ad, a compromised
//! site — could `fetch("http://127.0.0.1:4117/api/arm", { method: "POST" })`
//! and the browser would just let it through. This token stops that: every
//! route except the one that hands out the token itself requires it.
//!
//! What this does NOT protect against, to be precise about it: anything
//! with filesystem access to the token file (native malware running as the
//! same user, a compromised browser extension with broad host/file
//! permissions) can read `.sniper-token` directly and impersonate the UI —
//! this is a local-agent auth model, not a defense against a fully
//! compromised machine. Step 7b's own framing already named this precisely
//! as the gap: the token proves the CALLER IS THIS BROWSER TAB, not WHO IS
//! SITTING AT IT — anyone with the file is indistinguishable from the
//! operator. That's exactly the gap step 10 exists to close.
//!
//! ## Step 10g decision: retire the token as the base gate, don't keep it
//! alongside session auth
//!
//! `require_token_or_session` (used by `api.rs`'s `protected` router
//! instead of `require_token` directly) picks EXACTLY ONE mechanism per
//! boot, decided once by whether identity is configured
//! (`google_oauth_client_id` set) — never both active for the same
//! request, so there is no precedence question to get wrong:
//!
//! - **Identity NOT configured**: exactly step 7b's original bearer-token
//!   check, completely unchanged. An existing testnet-dry-run or CI setup
//!   that has never touched step 10 keeps working exactly as before —
//!   turning step 10's code on for other users never silently changes
//!   behavior for someone who hasn't configured it.
//! - **Identity configured**: the token is NOT checked here at all — a
//!   valid, `admin_tier` session (Google + TOTP + WebAuthn, all completed
//!   at login) is required instead. The considered alternative — "keep the
//!   token as a narrower fallback for local dev convenience" — was
//!   rejected: a fallback that's still a full bypass of per-identity auth
//!   on every fund-moving route defeats the entire point of building it,
//!   and "narrower" only in name if it's not actually scoped to a
//!   genuinely lower-stakes action. If a token-based local dev/CI path is
//!   ever needed again, it belongs on read-only endpoints specifically, as
//!   a new and explicitly-scoped decision — not as this token surviving
//!   unscoped once real auth exists.
//!
//! `.sniper-token`/`load_or_create_token` are UNCHANGED and still run even
//! when identity is configured — deleting the token-issuance path entirely
//! would break the moment someone disables identity again (e.g. to debug),
//! and the token file existing costs nothing when it's not the active gate.

use crate::identity::session;
use crate::state::SharedState;
use anyhow::{Context, Result};
use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use rand::RngCore;
use serde::Deserialize;
use std::fs;

/// Reads the local API token from `path`, generating and persisting a new
/// random one on first run. 32 random bytes, hex-encoded — plenty of
/// entropy for a token that only ever needs to resist guessing (it's
/// checked locally; there's no network-exposed brute-force surface).
pub fn load_or_create_token(path: &str) -> Result<String> {
    if let Ok(existing) = fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = alloy::hex::encode(bytes);

    fs::write(path, &token).with_context(|| format!("writing API token to {path}"))?;
    // Best-effort — not the primary defense (the file is gitignored and
    // lives next to config.toml, which already holds no secrets itself but
    // is treated the same way), just narrows the window on multi-user boxes.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }

    Ok(token)
}

#[derive(Deserialize)]
pub struct TokenQuery {
    token: Option<String>,
}

/// Bearer-token check shared by both the identity-not-configured path of
/// `require_token_or_session` and this module's own tests. Not a public
/// middleware in its own right anymore — see the step 10g decision in
/// this module's doc comment for why there is deliberately no standalone
/// token-only middleware left wired into the router.
///
/// Query param, not just header: browsers cannot set custom headers on a
/// `WebSocket` constructor call, so `/ws/events` has no way to send a
/// bearer header during the handshake. A `?token=` query param is the
/// simplest thing that works for both the WS upgrade and plain HTTP
/// requests, and is what's used here for both — a WebSocket subprotocol
/// would also work, but is fiddlier to negotiate correctly through axum/
/// tungstenite and more prone to being mishandled by intermediate proxies;
/// not worth it for a token that's already only meaningful on localhost.
fn bearer_token_matches(state: &SharedState, headers: &HeaderMap, query: &TokenQuery) -> bool {
    let header_ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == state.api_token);
    let query_ok = query.token.as_deref().is_some_and(|t| t == state.api_token);
    header_ok || query_ok
}

/// Base gate for `api.rs`'s `protected` router (step 10g) — see this
/// module's doc comment for the decision this implements. Picks bearer-
/// token or session auth, never both, based on whether identity is
/// configured for this boot.
///
/// Note for `/ws/events` specifically: a WebSocket upgrade request DOES
/// carry the browser's normal cookies (unlike the bearer token, which
/// needs the `?token=` query-param workaround above because custom
/// headers aren't settable on a `WebSocket` constructor call) — so the
/// session-cookie path here needs no equivalent workaround for that route.
pub async fn require_token_or_session(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    request: Request,
    next: Next,
) -> Response {
    if state.google_oidc.is_none() {
        if !bearer_token_matches(&state, &headers, &query) {
            return (StatusCode::UNAUTHORIZED, "missing or invalid API token").into_response();
        }
        return next.run(request).await;
    }

    let jar = PrivateCookieJar::from_headers(&headers, state.identity_cookie_key.clone());
    let Some(cookie) = jar.get(crate::api::SESSION_COOKIE) else {
        return (StatusCode::UNAUTHORIZED, "not signed in").into_response();
    };
    let active = match session::get_active(&state.identity_db, cookie.value()).await {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    match active {
        Some(s) if s.admin_tier => next.run(request).await,
        Some(_) => (
            StatusCode::FORBIDDEN,
            "session has not completed Google + TOTP + WebAuthn — this API requires a fully admin-tier session",
        )
            .into_response(),
        None => (StatusCode::UNAUTHORIZED, "not signed in").into_response(),
    }
}

#[cfg(test)]
mod tests {
    //! Router-level tests (step 10g) — `require_token_or_session` takes
    //! axum's `Next`, which can't be constructed by hand outside real
    //! router machinery, so these build a tiny real `Router` with the
    //! middleware layered on exactly like `api.rs`'s `protected` router
    //! does, and drive it with `tower::ServiceExt::oneshot` — this is
    //! what actually proves the route_layer wiring works, not just the
    //! inner logic in isolation.
    use super::*;
    use crate::identity::crypto;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use axum::Router;
    use axum_extra::extract::cookie::Cookie;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::{mpsc, RwLock};
    use tower::ServiceExt;

    async fn test_state(identity_configured: bool) -> SharedState {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("id.db");
        let key_path = dir.path().join(".session-key");
        Box::leak(Box::new(dir));
        let db = crate::db::open(db_path.to_str().unwrap()).await.unwrap();
        let keys = crypto::load_or_create(key_path.to_str().unwrap()).unwrap();

        let google_oidc = if identity_configured {
            Some(Arc::new(
                crate::identity::oidc::GoogleOidc::discover(
                    "test-client-id".to_string(),
                    "test-client-secret".to_string(),
                    "https://example.invalid/auth/google/callback".to_string(),
                )
                .await
                .expect("live discovery against accounts.google.com should succeed"),
            ))
        } else {
            None
        };

        let (control_tx, _control_rx) = mpsc::channel(8);
        Arc::new(crate::state::AppState {
            config: RwLock::new(crate::config::test_config()),
            wallet_status: RwLock::new(Vec::new()),
            armed: AtomicBool::new(false),
            bus: crate::bus::new_bus(),
            control_tx,
            config_path: "unused-in-this-test".to_string(),
            api_token: "the-real-token".to_string(),
            http_client: reqwest::Client::new(),
            identity_db: db,
            identity_cookie_key: keys.cookie_key,
            identity_totp_cipher: keys.totp_cipher,
            google_oidc,
            webauthn: None,
        })
    }

    fn guarded_router(state: SharedState) -> Router {
        Router::new()
            .route("/protected-thing", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_token_or_session))
            .with_state(state)
    }

    async fn make_admin_tier_session(state: &SharedState) -> String {
        let user_id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, 'sub', 'e@x.com', 0)")
            .bind(&user_id)
            .execute(&state.identity_db)
            .await
            .unwrap();
        let session_id = session::create_google_verified_session(&state.identity_db, &user_id).await.unwrap();
        sqlx::query("UPDATE sessions SET admin_tier = 1 WHERE id = ?")
            .bind(&session_id)
            .execute(&state.identity_db)
            .await
            .unwrap();
        session_id
    }

    fn cookie_value_for(state: &SharedState, session_id: &str) -> String {
        let jar = PrivateCookieJar::from_headers(&HeaderMap::new(), state.identity_cookie_key.clone())
            .add(Cookie::new(crate::api::SESSION_COOKIE, session_id.to_string()));
        let resp = jar.into_response();
        resp.headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn unconfigured_identity_accepts_valid_token_and_rejects_wrong_one() {
        let state = test_state(false).await;
        let app = guarded_router(state.clone());

        let ok = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/protected-thing")
                    .header(header::AUTHORIZATION, "Bearer the-real-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        let bad = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/protected-thing")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unconfigured_identity_ignores_a_session_cookie_and_still_needs_the_token() {
        // Proves the two mechanisms genuinely don't mix: even a real,
        // valid, admin_tier session cookie must NOT grant access when
        // identity is "configured" on the DB side but this AppState
        // instance itself has google_oidc = None (the actual gate) —
        // the token remains the ONLY thing checked in this mode.
        let state = test_state(false).await;
        let session_id = make_admin_tier_session(&state).await;
        let cookie = cookie_value_for(&state, &session_id);
        let app = guarded_router(state);

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/protected-thing")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn configured_identity_ignores_the_token_and_requires_a_session() {
        let state = test_state(true).await;
        let app = guarded_router(state);

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/protected-thing")
                    .header(header::AUTHORIZATION, "Bearer the-real-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "the bearer token must not grant access once identity is configured"
        );
    }

    #[tokio::test]
    async fn configured_identity_accepts_an_admin_tier_session() {
        let state = test_state(true).await;
        let session_id = make_admin_tier_session(&state).await;
        let cookie = cookie_value_for(&state, &session_id);
        let app = guarded_router(state);

        let resp = app
            .oneshot(HttpRequest::builder().uri("/protected-thing").header(header::COOKIE, cookie).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn configured_identity_rejects_a_non_admin_tier_session_with_403_not_401() {
        let state = test_state(true).await;
        let user_id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, 'sub2', 'e2@x.com', 0)")
            .bind(&user_id)
            .execute(&state.identity_db)
            .await
            .unwrap();
        let session_id = session::create_google_verified_session(&state.identity_db, &user_id).await.unwrap();
        let cookie = cookie_value_for(&state, &session_id);
        let app = guarded_router(state);

        let resp = app
            .oneshot(HttpRequest::builder().uri("/protected-thing").header(header::COOKIE, cookie).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
