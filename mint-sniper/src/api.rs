use crate::auth;
use crate::bus;
use crate::config::Config;
use crate::copymint;
use crate::identity::{session, totp};
use crate::opensea;
use crate::state::{ControlMsg, SharedState};
use crate::target;
use alloy::primitives::Address;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware,
    response::{IntoResponse, Redirect},
    routing::{get, post},
    Json, Router,
};
use axum_extra::extract::cookie::{Cookie, PrivateCookieJar, SameSite};
use serde::Deserialize;
use std::sync::atomic::Ordering;
use tower_http::cors::CorsLayer;

/// Cookie names — both httponly, both Secure (requires HTTPS; see
/// identity/oidc.rs's doc comment on why this flow assumes a Tailscale
/// MagicDNS HTTPS origin, not plain-HTTP localhost). SESSION_COOKIE
/// carries only the opaque session id (see identity/session.rs);
/// FLOW_COOKIE is short-lived, scoped to /auth/google, and never
/// outlives a single login attempt.
const SESSION_COOKIE: &str = "sniper_session";
const FLOW_COOKIE: &str = "sniper_oidc_flow";

/// Note: PUT /api/config accepts `wallets: [{ private_key_env: "SNIPER_PK_1" }]`
/// — env var *names* only. The UI never sees, sets, or transmits a raw
/// private key. Keys stay in the process environment on the machine
/// running the Rust binary. Do not "improve" this by adding a raw-key
/// field to the config JSON schema — that would put keys on the wire to
/// a browser tab, which is a materially worse security posture than a
/// TOML file on disk.
///
/// Auth: every route below requires the local bearer token (see auth.rs)
/// except GET /api/token, which is how the UI bootstraps it in the first
/// place, and the new /auth/google/* + /auth/logout routes (step 10c),
/// which are how a *session* gets established/torn down in the first
/// place and so can't themselves require one. The bearer token and
/// session-cookie auth currently coexist without either gating the
/// other — TODO(step 10g): make an explicit decision about whether the
/// static token is retired in favor of sessions or kept as a narrower
/// fallback; don't assume this dual-mechanism state is the final design.
/// CORS is an explicit allow-list, not `Any` — an arm/fire-capable
/// API bound to 127.0.0.1 is still reachable from any webpage open in the
/// same browser if CORS says any origin may call it; DNS-rebinding and
/// localhost-CSRF are real, known attack classes against exactly this
/// shape of unauthenticated local API. The token is the actual defense
/// (it also stops non-browser local callers, which CORS never could);
/// the allow-list narrows the browser-JS attack surface on top of that.
pub fn router(state: SharedState) -> Router {
    let protected = Router::new()
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/status", get(get_status))
        .route("/api/arm", post(post_arm))
        .route("/api/abort", post(post_abort))
        .route("/api/trigger", post(post_trigger))
        .route("/api/copymint/fire", post(post_copymint_fire))
        .route("/api/target/resolve", post(post_target_resolve))
        .route("/api/target/set", post(post_target_set))
        .route("/api/target/search", post(post_target_search))
        .route("/ws/events", get(ws_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_token));

    let public = Router::new()
        .route("/api/token", get(get_token))
        // Identity routes (step 10c) are deliberately outside the bearer-
        // token-protected `protected` router above: they're how a session
        // is established in the first place, so nothing gates them yet
        // except Google's own OAuth flow itself. Every route a session
        // actually needs to DO anything (arm/fire/config/etc.) stays
        // behind auth::require_token for now — see the TODO(step 10g) at
        // the top of this file's doc comment once 10g decides how the
        // bearer token and session auth coexist.
        .route("/auth/google/login", get(get_auth_google_login))
        .route("/auth/google/callback", get(get_auth_google_callback))
        .route("/auth/logout", post(post_auth_logout))
        .route("/auth/session", get(get_auth_session))
        .route("/auth/totp/setup/start", post(post_totp_setup_start))
        .route("/auth/totp/setup/verify", post(post_totp_setup_verify))
        .route("/auth/totp/verify", post(post_totp_verify));

    let dev_origin = HeaderValue::from_static("http://localhost:5173");
    let prod_origin_a = HeaderValue::from_static("http://127.0.0.1:4117");
    let prod_origin_b = HeaderValue::from_static("http://localhost:4117");

    protected
        .merge(public)
        .layer(
            CorsLayer::new()
                .allow_origin([dev_origin, prod_origin_a, prod_origin_b])
                .allow_methods([Method::GET, Method::POST, Method::PUT])
                .allow_headers([axum::http::header::CONTENT_TYPE, axum::http::header::AUTHORIZATION]),
        )
        .with_state(state)
}

/// The one unauthenticated route. Returns the local bearer token so the
/// UI can bootstrap it at startup (see ui/README.md's security note for
/// exactly what this does and doesn't protect against) — everything else
/// in this router requires it.
async fn get_token(State(state): State<SharedState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "token": state.api_token }))
}

async fn get_config(State(state): State<SharedState>) -> Json<Config> {
    Json(state.config.read().await.clone())
}

async fn put_config(
    State(state): State<SharedState>,
    Json(new_cfg): Json<Config>,
) -> impl IntoResponse {
    // Reject before touching in-memory state or disk — a bad PUT should
    // fail cleanly with a reason, not silently overwrite a working config
    // with something that'll only surface as a confusing failure at
    // arm/fire time. See Config::validate's doc comment for exactly what
    // is and isn't checked.
    if let Err(e) = new_cfg.validate() {
        bus::log(&state.bus, "error", format!("rejected config update: {e:#}"));
        return (StatusCode::BAD_REQUEST, format!("invalid config: {e:#}")).into_response();
    }

    {
        let mut cfg = state.config.write().await;
        *cfg = new_cfg.clone();
    }
    match toml::to_string_pretty(&new_cfg) {
        Ok(toml_str) => {
            if let Err(e) = tokio::fs::write(&state.config_path, toml_str).await {
                bus::log(&state.bus, "error", format!("config saved in memory but disk write failed: {e}"));
                return (StatusCode::INTERNAL_SERVER_ERROR, "in-memory only, disk write failed").into_response();
            }
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize failed: {e}")).into_response();
        }
    }
    bus::log(&state.bus, "info", "config updated via UI");
    let _ = state.bus.send(bus::ServerEvent::ConfigChanged);
    StatusCode::OK.into_response()
}

async fn get_status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let wallets = state.wallet_status.read().await.clone();
    Json(serde_json::json!({
        "armed": state.armed.load(Ordering::Relaxed),
        "wallets": wallets,
    }))
}

async fn post_arm(State(state): State<SharedState>) -> impl IntoResponse {
    let _ = state.control_tx.send(ControlMsg::Arm).await;
    StatusCode::ACCEPTED
}

async fn post_abort(State(state): State<SharedState>) -> impl IntoResponse {
    let _ = state.control_tx.send(ControlMsg::Disarm).await;
    StatusCode::ACCEPTED
}

async fn post_trigger(State(state): State<SharedState>) -> impl IntoResponse {
    bus::log(&state.bus, "warn", "manual FIRE requested from UI");
    let _ = state.control_tx.send(ControlMsg::FireNow).await;
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
struct CopymintFireRequest {
    nft_contract: String,
    fee_recipient: String,
}

/// Manual fire for a specific copymint opportunity — the ONLY path a
/// paid opportunity can ever fire through (copymint.rs's watcher
/// structurally cannot auto-fire one, see its doc comment). Deliberately
/// does NOT read `copymint_auto_fire_paid`: that flag's whole job is
/// telling the UI whether to show/enable the button that calls this
/// route in the first place, not gating this route itself — once a human
/// has authenticated and clicked fire, that IS the manual confirmation
/// this whole design exists to require. Also deliberately does NOT trust
/// a client-echoed price for the firing decision: `copymint::verify_and_fire`
/// re-fetches getPublicDrop fresh and re-checks liveness + the
/// max_copymint_price_wei ceiling right here, in case either changed
/// since the opportunity was first surfaced.
async fn post_copymint_fire(
    State(state): State<SharedState>,
    Json(req): Json<CopymintFireRequest>,
) -> impl IntoResponse {
    let nft_contract: Address = match req.nft_contract.parse() {
        Ok(a) => a,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad nft_contract: {e}")).into_response(),
    };
    let fee_recipient: Address = match req.fee_recipient.parse() {
        Ok(a) => a,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad fee_recipient: {e}")).into_response(),
    };

    match copymint::verify_and_fire(&state, nft_contract, fee_recipient).await {
        Ok(value) => (StatusCode::ACCEPTED, format!("firing — verified value {value} wei")).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

fn resolved_target_json(r: &target::ResolvedTarget, now: u64) -> serde_json::Value {
    serde_json::json!({
        "nft_contract": format!("{:#x}", r.nft_contract),
        "name": r.name,
        "links": r.links,
        "mint_price_wei": r.mint_price_wei.to_string(),
        "total_value_wei": r.total_value_wei().to_string(),
        "quantity_per_wallet": r.quantity_per_wallet,
        "start_time": r.start_time,
        "end_time": r.end_time,
        "max_per_wallet": r.max_per_wallet,
        "restrict_fee_recipients": r.restrict_fee_recipients,
        "fee_recipient": format!("{:#x}", r.fee_recipient),
        "fee_recipient_ok": r.fee_recipient_ok,
        "is_live": r.is_live(now),
        // A target can be a real, live drop and still not be settable —
        // either it's not live yet/anymore, or the configured
        // fee_recipient isn't accepted. The UI should gate its "set as
        // active target" action on this, not just fee_recipient_ok alone.
        "settable": r.is_live(now) && r.fee_recipient_ok,
    })
}

#[derive(Deserialize)]
struct TargetResolveRequest {
    input: String,
}

/// Read-only: resolves + verifies `input` (a raw address or OpenSea
/// collection URL) and returns the details, without changing any bot
/// state. A separate, explicit `/api/target/set` call is required to
/// actually make this the active target — same "verify, then surface,
/// then a separate commit action" shape copymint.rs already uses (see
/// its doc comment), and the same "no auto-arm on resolve" principle
/// every money-spending action in this codebase follows.
async fn post_target_resolve(
    State(state): State<SharedState>,
    Json(req): Json<TargetResolveRequest>,
) -> impl IntoResponse {
    match target::resolve(&state, &state.http_client, &req.input).await {
        Ok(resolved) => Json(resolved_target_json(&resolved, bus::now_ts())).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct TargetSetRequest {
    nft_contract: String,
}

// TODO(step 10f — identity/step-up auth, not merged as of this writing):
// this route changes where the bot's next mint sends money, the same
// sensitivity class as /api/arm and /api/trigger. It currently only
// requires the same local bearer token every other route needs. When
// step 10's step-up auth lands, this route should require it too — don't
// let this get missed just because it shipped before step 10 did.
//
/// Actually swaps the active seadrop target. Deliberately does NOT trust
/// anything the client sent beyond `nft_contract` — re-runs the full
/// `target::resolve_address` verification fresh (never a cached result
/// from an earlier `/resolve` call, which could be stale by the time the
/// operator clicks confirm) and refuses to proceed unless the drop is
/// both live and `fee_recipient_ok`. Only seadrop mode is supported —
/// custom mode has no `getPublicDrop`/collection concept to resolve
/// against. On success: sends `ControlMsg::SetTarget` (see its doc
/// comment in state.rs for the control_loop-side cleanup this triggers)
/// and persists `nft_contract` into `config.toml` via the same
/// validate-then-write path `PUT /api/config` uses.
async fn post_target_set(
    State(state): State<SharedState>,
    Json(req): Json<TargetSetRequest>,
) -> impl IntoResponse {
    let cfg = state.config.read().await.clone();

    if cfg.mint_mode != "seadrop" {
        return (
            StatusCode::BAD_REQUEST,
            "target resolution/set is only supported for mint_mode = \"seadrop\"".to_string(),
        )
            .into_response();
    }

    let nft_contract: Address = match req.nft_contract.parse() {
        Ok(a) => a,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad nft_contract: {e}")).into_response(),
    };

    let resolved = match target::resolve_address(&cfg, nft_contract).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    };

    let now = bus::now_ts();
    if !resolved.is_live(now) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "drop is not currently live (start {}, end {}, now {})",
                resolved.start_time, resolved.end_time, now
            ),
        )
            .into_response();
    }
    if !resolved.fee_recipient_ok {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "configured fee_recipient {:#x} is not accepted on this drop \
                 (restrictFeeRecipients=true) — update fee_recipient before setting this target",
                resolved.fee_recipient
            ),
        )
            .into_response();
    }

    let mint_calldata = match resolved.mint_calldata() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("encoding mintPublic calldata failed: {e}")).into_response(),
    };
    let mint_value = resolved.total_value_wei();

    // Persist nft_contract into config.toml — same validate-then-write
    // path PUT /api/config uses, so this and a manual config edit get
    // identical treatment (7f's validation applies here too).
    let mut new_cfg = cfg.clone();
    new_cfg.nft_contract = format!("{nft_contract:#x}");
    if let Err(e) = new_cfg.validate() {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("resolved target failed config validation: {e:#}")).into_response();
    }
    {
        let mut w = state.config.write().await;
        *w = new_cfg.clone();
    }
    match toml::to_string_pretty(&new_cfg) {
        Ok(toml_str) => {
            if let Err(e) = tokio::fs::write(&state.config_path, toml_str).await {
                bus::log(&state.bus, "error", format!("target set in memory but disk write failed: {e}"));
            }
        }
        Err(e) => bus::log(&state.bus, "error", format!("target set in memory but config serialize failed: {e}")),
    }
    let _ = state.bus.send(bus::ServerEvent::ConfigChanged);

    let _ = state
        .control_tx
        .send(ControlMsg::SetTarget { nft_contract, mint_calldata, mint_value })
        .await;

    Json(resolved_target_json(&resolved, now)).into_response()
}

#[derive(Deserialize)]
struct TargetSearchRequest {
    query: String,
}

/// How many candidates to return, per 8c's "top 5-10 candidates"
/// instruction — the low end of that range, since every candidate is
/// unverified and the UI fetches full verification details only for
/// whichever one the operator actually selects (see TargetSearch.tsx),
/// so a larger number here only means more unverified noise in the
/// picklist, not more safety.
const SEARCH_RESULT_LIMIT: u8 = 8;

/// Free-text collection name search (step 8c). Read-only, same as
/// resolve — returns unverified candidates (name/slug/image only, per
/// OpenSea's own search response shape) for the operator to pick from.
/// NONE of these are pre-verified: picking one still requires a separate
/// `/api/target/resolve` (or going straight to `/api/target/set`, which
/// re-verifies regardless) before it means anything. See opensea.rs's
/// doc comment for the full namesquatting-risk reasoning this design is
/// built around — OpenSea's result order is never treated as a trust
/// signal here or anywhere downstream of this handler.
async fn post_target_search(
    State(state): State<SharedState>,
    Json(req): Json<TargetSearchRequest>,
) -> impl IntoResponse {
    let cfg = state.config.read().await.clone();
    let api_key = match cfg.resolve_opensea_api_key() {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "name search needs an OpenSea API key (opensea_api_key_env in config is unset or \
                 the env var it names isn't set) — a name search has no zero-key path, unlike a \
                 raw address or a pasted OpenSea URL"
                    .to_string(),
            )
                .into_response();
        }
    };

    match opensea::search_collections(&state.http_client, &api_key, &req.query, SEARCH_RESULT_LIMIT).await {
        Ok(hits) => Json(hits).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

// --- identity (step 10c) ---
//
// PrivateCookieJar is built manually via `from_headers` rather than as an
// axum extractor argument — see state.rs's NOTE for why: SharedState is
// `Arc<AppState>`, and axum-extra's FromRef-based extraction needs a
// local (non-Arc-wrapped) state type, which would mean deviating from
// this codebase's existing Arc<AppState> convention everywhere else.

/// Redirects the browser to Google's consent screen. 503s with a clear
/// message if Google Sign-In isn't configured, rather than 404ing (which
/// would look like a routing bug) or panicking.
async fn get_auth_google_login(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    let Some(oidc) = &state.google_oidc else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Google Sign-In is not configured — set google_oauth_client_id/_secret_env/_redirect_url in config.toml",
        )
            .into_response();
    };

    let start = oidc.begin_login().await;
    let jar = PrivateCookieJar::from_headers(&headers, state.identity_cookie_key.clone());
    let flow_cookie = Cookie::build((FLOW_COOKIE, start.flow_id))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .path("/auth/google")
        .build();
    let jar = jar.add(flow_cookie);

    (jar, Redirect::to(&start.auth_url)).into_response()
}

#[derive(Deserialize)]
struct GoogleCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Google's redirect target after the user approves/denies consent.
/// Validates CSRF state + PKCE + the ID token's nonce (all inside
/// `oidc.complete_login`), upserts the user by `google_sub`, and issues a
/// new session at the "Google done, TOTP/WebAuthn pending" stage — see
/// identity/session.rs's doc comment for how that session gets promoted
/// to admin_tier by 10d/10e.
async fn get_auth_google_callback(
    State(state): State<SharedState>,
    Query(q): Query<GoogleCallbackQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(oidc) = &state.google_oidc else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Google Sign-In is not configured").into_response();
    };

    if let Some(err) = q.error {
        return (StatusCode::BAD_REQUEST, format!("Google sign-in was not completed: {err}")).into_response();
    }
    let (Some(code), Some(oauth_state)) = (q.code, q.state) else {
        return (StatusCode::BAD_REQUEST, "callback missing code/state query params").into_response();
    };

    let jar = PrivateCookieJar::from_headers(&headers, state.identity_cookie_key.clone());
    let Some(flow_cookie) = jar.get(FLOW_COOKIE) else {
        return (
            StatusCode::BAD_REQUEST,
            "missing or expired login flow cookie — start over at /auth/google/login",
        )
            .into_response();
    };
    let flow_id = flow_cookie.value().to_string();
    let jar = jar.remove(Cookie::from(FLOW_COOKIE));

    let claims = match oidc.complete_login(&flow_id, code, oauth_state).await {
        Ok(c) => c,
        Err(e) => {
            bus::log(&state.bus, "warn", format!("google sign-in attempt failed: {e:#}"));
            return (StatusCode::UNAUTHORIZED, jar, format!("sign-in failed: {e:#}")).into_response();
        }
    };

    let user_id = match session::find_or_create_user(&state.identity_db, &claims.google_sub, &claims.email).await {
        Ok(id) => id,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, jar, format!("{e:#}")).into_response(),
    };
    let session_id = match session::create_google_verified_session(&state.identity_db, &user_id).await {
        Ok(id) => id,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, jar, format!("{e:#}")).into_response(),
    };

    bus::log(&state.bus, "info", format!("google sign-in succeeded for {}", claims.email));

    let session_cookie = Cookie::build((SESSION_COOKIE, session_id))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .path("/")
        .build();
    let jar = jar.add(session_cookie);

    // Where the browser lands next (TOTP entry, WebAuthn prompt, or
    // straight to the dashboard) is the SPA's job from here — it reads
    // /auth/session below and decides what to show.
    (jar, Redirect::to("/")).into_response()
}

async fn post_auth_logout(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    let jar = PrivateCookieJar::from_headers(&headers, state.identity_cookie_key.clone());
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        if let Err(e) = session::revoke(&state.identity_db, cookie.value()).await {
            bus::log(&state.bus, "error", format!("logout: revoking session failed: {e:#}"));
        }
    }
    let jar = jar.remove(Cookie::from(SESSION_COOKIE));
    (jar, StatusCode::OK)
}

/// Read-only session-state check for the SPA — "am I signed in, and how
/// far through the Google/TOTP/WebAuthn chain is this session." Never
/// requires the bearer token (a not-yet-admin-tier session has no token
/// yet either), and deliberately returns the same "not signed in" shape
/// whether the cookie is missing, unparseable, or points at a
/// revoked/nonexistent session — same "don't distinguish absent from
/// revoked" reasoning as identity/session.rs's get_active.
async fn get_auth_session(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    let jar = PrivateCookieJar::from_headers(&headers, state.identity_cookie_key.clone());
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Json(serde_json::json!({ "signed_in": false }));
    };
    match session::get_active(&state.identity_db, cookie.value()).await {
        Ok(Some(s)) => Json(serde_json::json!({
            "signed_in": true,
            "admin_tier": s.admin_tier,
            "totp_verified": s.totp_verified_at.is_some(),
            "webauthn_verified": s.webauthn_verified_at.is_some(),
        })),
        _ => Json(serde_json::json!({ "signed_in": false })),
    }
}

/// Extracts the caller's active session from the session cookie, or a
/// 401. Used by every route that needs "a signed-in user" but not
/// necessarily admin_tier yet — TOTP/WebAuthn setup and per-login
/// verification are exactly how a session PROGRESSES toward admin_tier,
/// so they can't themselves require it. Routes that DO need admin_tier
/// (once 10f's step-up auth exists) will check `session.admin_tier`
/// explicitly on top of this, not replace it.
async fn require_session(state: &SharedState, headers: &HeaderMap) -> Result<session::Session, (StatusCode, String)> {
    let jar = PrivateCookieJar::from_headers(headers, state.identity_cookie_key.clone());
    let cookie = jar
        .get(SESSION_COOKIE)
        .ok_or((StatusCode::UNAUTHORIZED, "not signed in".to_string()))?;
    session::get_active(&state.identity_db, cookie.value())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .ok_or((StatusCode::UNAUTHORIZED, "not signed in".to_string()))
}

/// Generates a fresh TOTP secret for the signed-in user and returns a QR
/// code + base32 manual-entry fallback (step 10d). Does NOT enable
/// anything yet — see totp.rs's start_setup doc comment.
async fn post_totp_setup_start(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let email = match session::get_user_email(&state.identity_db, &session.user_id).await {
        Ok(e) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    match totp::start_setup(&state.identity_db, &state.identity_totp_cipher, &session.user_id, &email).await {
        Ok(material) => Json(serde_json::json!({
            "qr_data_uri": material.qr_data_uri,
            "secret_base32": material.secret_base32,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct TotpCodeRequest {
    code: String,
}

/// Completes TOTP setup: proves the operator's authenticator app has a
/// working copy of the secret from `start_setup`, before anything ever
/// treats TOTP as "on". Also marks THIS session's TOTP stage done — the
/// live verification a fresh setup requires doubles as this session's
/// per-login TOTP check, since it's the exact same proof.
async fn post_totp_setup_verify(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<TotpCodeRequest>,
) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let email = match session::get_user_email(&state.identity_db, &session.user_id).await {
        Ok(e) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    match totp::verify_setup(&state.identity_db, &state.identity_totp_cipher, &session.user_id, &email, &req.code).await {
        Ok(true) => {
            if let Err(e) = session::mark_totp_verified(&state.identity_db, &session.id).await {
                bus::log(&state.bus, "error", format!("totp setup verified but marking session failed: {e:#}"));
            }
            bus::log(&state.bus, "info", "TOTP 2FA enabled");
            StatusCode::OK.into_response()
        }
        Ok(false) => (StatusCode::UNAUTHORIZED, "incorrect code").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// Per-login TOTP check for a user whose secret is already enabled from
/// a prior setup — see totp.rs's verify_login for the replay-protection
/// details (a code can only ever be accepted once).
async fn post_totp_verify(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<TotpCodeRequest>,
) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let email = match session::get_user_email(&state.identity_db, &session.user_id).await {
        Ok(e) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    match totp::verify_login(&state.identity_db, &state.identity_totp_cipher, &session.user_id, &email, &req.code).await {
        Ok(true) => {
            if let Err(e) = session::mark_totp_verified(&state.identity_db, &session.id).await {
                bus::log(&state.bus, "error", format!("totp verified but marking session failed: {e:#}"));
            }
            StatusCode::OK.into_response()
        }
        Ok(false) => (StatusCode::UNAUTHORIZED, "incorrect or already-used code").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<SharedState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| stream_events(socket, state))
}

async fn stream_events(mut socket: WebSocket, state: SharedState) {
    let mut rx = state.bus.subscribe();
    // Replay current status immediately on connect so a UI that opens
    // mid-session isn't blank until the next event fires.
    if let Ok(snapshot) = serde_json::to_string(&serde_json::json!({
        "type": "snapshot",
        "armed": state.armed.load(Ordering::Relaxed),
        "wallets": *state.wallet_status.read().await,
    })) {
        let _ = socket.send(Message::Text(snapshot)).await;
    }

    while let Ok(event) = rx.recv().await {
        if let Ok(json) = serde_json::to_string(&event) {
            if socket.send(Message::Text(json)).await.is_err() {
                break; // client disconnected
            }
        }
    }
}
