//! Local bearer-token auth for the control API.
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
//! compromised machine.

use crate::state::SharedState;
use anyhow::{Context, Result};
use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
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

/// Requires `Authorization: Bearer <token>` or a `?token=` query param
/// matching `AppState::api_token`. Applied to every route except
/// `GET /api/token` (see `api.rs`'s router) — that route is how the UI
/// bootstraps the token in the first place, so it can't require it too.
///
/// Query param, not just header: browsers cannot set custom headers on a
/// `WebSocket` constructor call, so `/ws/events` has no way to send a
/// bearer header during the handshake. A `?token=` query param is the
/// simplest thing that works for both the WS upgrade and plain HTTP
/// requests, and is what's used here for both — a WebSocket subprotocol
/// would also work, but is fiddlier to negotiate correctly through axum/
/// tungstenite and more prone to being mishandled by intermediate proxies;
/// not worth it for a token that's already only meaningful on localhost.
pub async fn require_token(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    request: Request,
    next: Next,
) -> Response {
    let header_ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == state.api_token);

    let query_ok = query.token.as_deref().is_some_and(|t| t == state.api_token);

    if !header_ok && !query_ok {
        return (StatusCode::UNAUTHORIZED, "missing or invalid API token").into_response();
    }

    next.run(request).await
}
