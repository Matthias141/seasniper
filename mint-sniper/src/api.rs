use crate::auth;
use crate::bus;
use crate::config::Config;
use crate::copymint;
use crate::identity::{session, totp};
use webauthn_rs::prelude::{PublicKeyCredential, RegisterPublicKeyCredential};
use crate::opensea;
use crate::state::{ControlMsg, SharedState};
use crate::target;
use alloy::primitives::Address;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
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
pub(crate) const SESSION_COOKIE: &str = "sniper_session";
const FLOW_COOKIE: &str = "sniper_oidc_flow";

/// Note: PUT /api/config accepts `wallets: [{ private_key_env: "SNIPER_PK_1" }]`
/// — env var *names* only. The UI never sees, sets, or transmits a raw
/// private key. Keys stay in the process environment on the machine
/// running the Rust binary. Do not "improve" this by adding a raw-key
/// field to the config JSON schema — that would put keys on the wire to
/// a browser tab, which is a materially worse security posture than a
/// TOML file on disk.
///
/// Auth: every route below requires `auth::require_token_or_session`
/// (step 10g) — see auth.rs's module doc comment for the full decision,
/// summary here: exactly ONE of bearer-token or session auth is active
/// per boot, decided once by whether identity (step 10) is configured,
/// never both for the same request. GET /api/token (how the UI
/// bootstraps the token when identity isn't configured) and the
/// /auth/google/* + /auth/logout + /auth/session + /auth/totp/* +
/// /auth/webauthn/* routes (step 10c-10e — how a session gets
/// established/torn down/progressed in the first place, so they can't
/// themselves require one) are the only routes outside this gate.
/// CORS is an explicit allow-list, not `Any` — an arm/fire-capable
/// API bound to 127.0.0.1 is still reachable from any webpage open in the
/// same browser if CORS says any origin may call it; DNS-rebinding and
/// localhost-CSRF are real, known attack classes against exactly this
/// shape of unauthenticated local API. The token is the actual defense
/// (it also stops non-browser local callers, which CORS never could);
/// the allow-list narrows the browser-JS attack surface on top of that.
/// `google_oauth_redirect_url` (empty if identity isn't configured) is
/// passed in explicitly, not read from `state.config` here, because CORS
/// origins are decided once at boot — same "decided once, not re-derived
/// per request" shape as `auth::require_token_or_session`'s own mode
/// choice. When set, its origin (scheme+host+port — see
/// `identity::webauthn::derive_origin`, the same parse WebAuthn's
/// rp_origin uses, so this can never drift from it) is added as a fourth
/// explicit CORS origin alongside the three step 7b ones below — step
/// 10.5b's Cloudflare Tunnel hostname, added here rather than as a
/// separate config field so there remains exactly ONE place this
/// codebase decides "this is our public origin."
pub fn router(state: SharedState, google_oauth_redirect_url: &str) -> Router {
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
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_token_or_session));

    let public = Router::new()
        .route("/api/token", get(get_token))
        // Identity routes (step 10c-10e) are deliberately outside
        // `protected` above: they're how a session gets established/
        // progressed/torn down in the first place, so nothing but
        // Google's own OAuth flow (and, per-route, their own internal
        // require_session/require_step_up checks) gates them. Every
        // route that actually DOES something (arm/fire/config/etc.)
        // sits behind require_token_or_session instead (step 10g).
        .route("/auth/google/login", get(get_auth_google_login))
        .route("/auth/google/callback", get(get_auth_google_callback))
        .route("/auth/logout", post(post_auth_logout))
        .route("/auth/session", get(get_auth_session))
        .route("/auth/totp/setup/start", post(post_totp_setup_start))
        .route("/auth/totp/setup/verify", post(post_totp_setup_verify))
        .route("/auth/totp/verify", post(post_totp_verify))
        .route("/auth/webauthn/register/start", post(post_webauthn_register_start))
        .route("/auth/webauthn/register/finish", post(post_webauthn_register_finish))
        .route("/auth/webauthn/authenticate/start", post(post_webauthn_authenticate_start))
        .route("/auth/webauthn/authenticate/finish", post(post_webauthn_authenticate_finish))
        .route("/auth/webauthn/devices", get(get_webauthn_devices))
        .route("/auth/webauthn/devices/:id/revoke", post(post_webauthn_device_revoke));

    let dev_origin = HeaderValue::from_static("http://localhost:5173");
    let prod_origin_a = HeaderValue::from_static("http://127.0.0.1:4117");
    let prod_origin_b = HeaderValue::from_static("http://localhost:4117");

    let mut allowed_origins = vec![dev_origin, prod_origin_a, prod_origin_b];
    if !google_oauth_redirect_url.is_empty() {
        // Errors here mean `Config::validate` didn't run or didn't catch
        // a bad URL — fail loudly at boot rather than silently serving
        // with a narrower-than-intended CORS allow-list (the same
        // "broken config should fail startup, not degrade quietly"
        // stance main.rs already takes for Google Sign-In/WebAuthn init).
        let origin = crate::identity::webauthn::derive_origin(google_oauth_redirect_url)
            .expect("google_oauth_redirect_url must already be a valid URL by the time router() is called — Config::validate should have caught this");
        allowed_origins.push(
            HeaderValue::from_str(origin.as_str().trim_end_matches('/'))
                .expect("a parsed URL's origin string is always a valid header value"),
        );
    }

    protected
        .merge(public)
        .layer(
            CorsLayer::new()
                .allow_origin(allowed_origins)
                .allow_methods([Method::GET, Method::POST, Method::PUT])
                .allow_headers([axum::http::header::CONTENT_TYPE, axum::http::header::AUTHORIZATION]),
        )
        .with_state(state)
        // STEP 15b — ui/README.md's "Prod" section has described this as
        // the plan since before step 10, but it was never actually wired
        // up until now (found while preparing this repo's first-ever
        // real off-sandbox deploy — a real gap, not a hypothetical one,
        // since a deploy script assuming this worked would have left the
        // operator with no way to serve the UI at all). `ServeDir` for
        // static assets, falling back to `index.html` for anything it
        // doesn't recognize — this app has no client-side router (no
        // react-router — confirmed by checking, not assumed) so that
        // fallback exists purely for PWA reload-to-a-cached-path safety,
        // not because multiple real routes need it. Deliberately OUTSIDE
        // `require_token_or_session`: the static shell (HTML/JS/CSS) must
        // be loadable BEFORE the browser can call `GET /api/token` or
        // check session state at all — auth applies to the API calls the
        // shell makes, never to the shell itself, same boundary
        // `AuthGate` in App.tsx already assumes client-side. Path is
        // relative to CWD ("ui/dist"), same convention as config.toml/
        // .sniper-token/identity.db — the binary must be run from the
        // repo root, per RUNBOOK.md and the step 15b deploy checklist.
        .fallback_service(
            tower_http::services::ServeDir::new("ui/dist")
                .fallback(tower_http::services::ServeFile::new("ui/dist/index.html")),
        )
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
    headers: HeaderMap,
    Json(new_cfg): Json<Config>,
) -> impl IntoResponse {
    // Step-up auth (10f) — config includes wallets/mint targets/trigger
    // settings, the same sensitivity class as arming or firing directly.
    if let Err((code, msg)) = require_step_up(&state, &headers).await {
        return (code, msg).into_response();
    }

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

async fn post_arm(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    // Step-up auth (10f) — arming starts a watcher that will auto-fire
    // real money on trigger, the same sensitivity class as firing itself.
    if let Err((code, msg)) = require_step_up(&state, &headers).await {
        return (code, msg).into_response();
    }
    let _ = state.control_tx.send(ControlMsg::Arm).await;
    StatusCode::ACCEPTED.into_response()
}

// Deliberately NOT step-up-gated: disarming only ever makes the bot do
// LESS (stop watching, stop firing) — there is no "quiet, dangerous"
// interpretation of disarm the way there is for arm/fire/config/target,
// and step-up-gating an emergency stop would be actively harmful if the
// operator's authenticator app were unavailable at exactly the wrong
// moment. See CLAUDE.md's "Identity (step 10)" section for this being a
// deliberate scope boundary, not an oversight.
async fn post_abort(State(state): State<SharedState>) -> impl IntoResponse {
    let _ = state.control_tx.send(ControlMsg::Disarm).await;
    StatusCode::ACCEPTED
}

async fn post_trigger(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err((code, msg)) = require_step_up(&state, &headers).await {
        return (code, msg).into_response();
    }
    bus::log(&state.bus, "warn", "manual FIRE requested from UI");
    let _ = state.control_tx.send(ControlMsg::FireNow).await;
    StatusCode::ACCEPTED.into_response()
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
    headers: HeaderMap,
    Json(req): Json<CopymintFireRequest>,
) -> impl IntoResponse {
    if let Err((code, msg)) = require_step_up(&state, &headers).await {
        return (code, msg).into_response();
    }

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
        // STEP 29b — additional signals alongside the existing
        // namesquatting warning; neither blocks resolution or replaces
        // human judgment. contract_age_secs is null when it couldn't be
        // determined — the UI must not read null as "old and safe."
        "contract_age_secs": r.contract_age_secs,
        "goplus_malicious": r.goplus.malicious_nft_contract,
        "goplus_create_block_number": r.goplus.create_block_number,
        "goplus_concerning": r.goplus.is_concerning(),
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

/// Actually swaps the active seadrop target. Step-up-gated (10f) —
/// resolved TODO from step 8b: this route changes where the bot's next
/// mint sends money, the same sensitivity class as /api/arm and
/// /api/trigger. Deliberately does NOT trust anything the client sent
/// beyond `nft_contract` — re-runs the full `target::resolve_address`
/// verification fresh (never a cached result from an earlier `/resolve`
/// call, which could be stale by the time the operator clicks confirm)
/// and refuses to proceed unless the drop is both live and
/// `fee_recipient_ok`. Only seadrop mode is supported — custom mode has
/// no `getPublicDrop`/collection concept to resolve against. On success:
/// sends `ControlMsg::SetTarget` (see its doc comment in state.rs for
/// the control_loop-side cleanup this triggers) and persists
/// `nft_contract` into `config.toml` via the same validate-then-write
/// path `PUT /api/config` uses.
async fn post_target_set(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<TargetSetRequest>,
) -> impl IntoResponse {
    if let Err((code, msg)) = require_step_up(&state, &headers).await {
        return (code, msg).into_response();
    }

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
    let identity_configured = state.google_oidc.is_some();
    let jar = PrivateCookieJar::from_headers(&headers, state.identity_cookie_key.clone());
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Json(serde_json::json!({ "signed_in": false, "identity_configured": identity_configured })).into_response();
    };
    let Ok(Some(s)) = session::get_active(&state.identity_db, cookie.value()).await else {
        return Json(serde_json::json!({ "signed_in": false, "identity_configured": identity_configured })).into_response();
    };

    // Distinct from `totp_verified`/`webauthn_verified` (THIS session's
    // login-stage progress): whether the ACCOUNT has an enrolled,
    // enabled factor at all, regardless of session. The UI needs this to
    // decide "show a QR code to set up TOTP for the first time" vs
    // "just ask for the code from your already-set-up app" — reusing
    // /auth/totp/setup/start for an already-enrolled account would
    // silently regenerate (and invalidate) the operator's existing
    // authenticator entry every time they merely log back in.
    let totp_enrolled = totp_is_enrolled(&state.identity_db, &s.user_id).await.unwrap_or(false);
    let webauthn_enrolled = crate::identity::webauthn::list_devices(&state.identity_db, &s.user_id)
        .await
        .map(|d| !d.is_empty())
        .unwrap_or(false);

    Json(serde_json::json!({
        "signed_in": true,
        "identity_configured": identity_configured,
        "admin_tier": s.admin_tier,
        "totp_verified": s.totp_verified_at.is_some(),
        "webauthn_verified": s.webauthn_verified_at.is_some(),
        "totp_enrolled": totp_enrolled,
        "webauthn_enrolled": webauthn_enrolled,
    }))
    .into_response()
}

async fn totp_is_enrolled(pool: &sqlx::SqlitePool, user_id: &str) -> anyhow::Result<bool> {
    let enabled: Option<bool> = sqlx::query_scalar("SELECT enabled FROM totp_secrets WHERE user_id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(enabled.unwrap_or(false))
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
    let session = session::get_active(&state.identity_db, cookie.value())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .ok_or((StatusCode::UNAUTHORIZED, "not signed in".to_string()))?;
    // Best-effort freshness bump — failing to record this shouldn't fail
    // the request itself, so errors are swallowed (there's nothing more
    // useful to do with a DB write failure on a non-critical timestamp
    // than to note it and move on).
    if let Err(e) = session::touch(&state.identity_db, &session.id).await {
        bus::log(&state.bus, "warn", format!("touching session last_seen_at failed: {e:#}"));
    }
    Ok(session)
}

/// Step-up auth (step 10f). A valid, admin_tier session (Google + TOTP +
/// a recognized WebAuthn device, all completed at LOGIN time) is enough
/// to VIEW — `get_status`/`ws_handler` never call this. It is NOT enough
/// to arm/fire/change config/change target: those additionally require a
/// FRESH TOTP code submitted on that specific request, via the
/// `X-Step-Up-Totp` header, checked through the exact same
/// `totp::verify_login` (and its replay protection — see totp.rs's
/// migration 0003) that a real login uses. A stale session doesn't get a
/// pass just because it's old; a code doesn't get reused just because it
/// worked a minute ago for something else.
///
/// Returns a DISTINCT, specific error for each failure mode — this is a
/// deliberately testable property (see this module's test coverage
/// below), not just "401 for everything":
/// - no session at all → 401 (from `require_session`)
/// - a real but non-admin-tier session (Google done, TOTP/WebAuthn still
///   pending) → 403, a different failure than "not signed in"
/// - missing `X-Step-Up-Totp` header entirely → 428 Precondition Required
/// - present but wrong/expired/already-used code → 412 Precondition Failed
///
/// If identity isn't configured at all (`google_oauth_client_id` unset),
/// step-up is inapplicable and these routes fall back to exactly their
/// pre-10f behavior (bearer-token-gated only) — same "opt-in, never
/// breaks an existing non-identity setup" convention as every other
/// step 10 route.
async fn require_step_up(state: &SharedState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    if state.google_oidc.is_none() {
        return Ok(());
    }

    let session = require_session(state, headers).await?;
    if !session.admin_tier {
        return Err((
            StatusCode::FORBIDDEN,
            "session has not completed Google + TOTP + WebAuthn — this action requires a fully admin-tier session".to_string(),
        ));
    }

    let code = headers
        .get("x-step-up-totp")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or((
            StatusCode::PRECONDITION_REQUIRED,
            "missing X-Step-Up-Totp header — this action requires a fresh TOTP code on this specific request".to_string(),
        ))?;

    let email = session::get_user_email(&state.identity_db, &session.user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;

    let ok = totp::verify_login(&state.identity_db, &state.identity_totp_cipher, &session.user_id, &email, code)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;

    if !ok {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "step-up TOTP code was incorrect, expired, or already used".to_string(),
        ));
    }

    Ok(())
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

// --- WebAuthn (step 10e) ---

async fn post_webauthn_register_start(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let Some(webauthn) = &state.webauthn else {
        return (StatusCode::SERVICE_UNAVAILABLE, "WebAuthn is not configured").into_response();
    };
    let email = match session::get_user_email(&state.identity_db, &session.user_id).await {
        Ok(e) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    match webauthn.start_registration(&state.identity_db, &session.id, &session.user_id, &email).await {
        Ok(ccr) => Json(ccr).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct WebauthnRegisterFinishRequest {
    device_label: String,
    credential: RegisterPublicKeyCredential,
}

async fn post_webauthn_register_finish(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<WebauthnRegisterFinishRequest>,
) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let Some(webauthn) = &state.webauthn else {
        return (StatusCode::SERVICE_UNAVAILABLE, "WebAuthn is not configured").into_response();
    };
    let label = req.device_label.trim();
    if label.is_empty() {
        return (StatusCode::BAD_REQUEST, "device_label must not be empty").into_response();
    }
    match webauthn
        .finish_registration(&state.identity_db, &session.id, &session.user_id, label, &req.credential)
        .await
    {
        Ok(()) => {
            bus::log(&state.bus, "info", format!("WebAuthn device registered: {label}"));
            StatusCode::OK.into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

async fn post_webauthn_authenticate_start(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let Some(webauthn) = &state.webauthn else {
        return (StatusCode::SERVICE_UNAVAILABLE, "WebAuthn is not configured").into_response();
    };
    match webauthn.start_authentication(&state.identity_db, &session.id, &session.user_id).await {
        Ok(rcr) => Json(rcr).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

async fn post_webauthn_authenticate_finish(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(cred): Json<PublicKeyCredential>,
) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let Some(webauthn) = &state.webauthn else {
        return (StatusCode::SERVICE_UNAVAILABLE, "WebAuthn is not configured").into_response();
    };
    match webauthn.finish_authentication(&state.identity_db, &session.id, &cred).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::UNAUTHORIZED, format!("{e:#}")).into_response(),
    }
}

async fn get_webauthn_devices(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    match crate::identity::webauthn::list_devices(&state.identity_db, &session.user_id).await {
        Ok(devices) => Json(devices).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// Revokes a device — the "revoke" half of 10e's "view and revoke"
/// requirement, and the answer to "what if I lose a device": the OTHER
/// registered device signs in, revokes the lost one, and (if desired)
/// registers a replacement, all without ever hitting the 2-device cap
/// (revoke first frees a slot). See RUNBOOK.md's "Lost both WebAuthn
/// devices" entry for the harder case this route can't self-service.
async fn post_webauthn_device_revoke(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    let session = match require_session(&state, &headers).await {
        Ok(s) => s,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    match crate::identity::webauthn::revoke_device(&state.identity_db, &session.user_id, &device_id).await {
        Ok(true) => {
            bus::log(&state.bus, "warn", format!("WebAuthn device revoked (id {device_id})"));
            StatusCode::OK.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such device for this account").into_response(),
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

#[cfg(test)]
mod step_up_tests {
    //! Step-up auth (10f) as a real, testable property — same standard
    //! copymint's 6e safety tests set: a request to a fund-moving route
    //! with a stale or missing step-up token must fail with a clear,
    //! DISTINCT error, not a generic 401. Builds a real `SharedState`
    //! (real identity DB, real cookie/TOTP keys, a real `GoogleOidc` from
    //! live discovery — same network dependency 10c's own test already
    //! established) and calls `require_step_up` directly rather than
    //! spinning up a full HTTP server, since it's a plain async fn in
    //! this module.
    use super::*;
    use crate::identity::{crypto, oidc};
    use axum_extra::extract::cookie::Cookie;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::{mpsc, RwLock};

    async fn test_state(identity_configured: bool) -> SharedState {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("id.db");
        let key_path = dir.path().join(".session-key");
        Box::leak(Box::new(dir)); // keep the TempDir (and its directory) alive for the test's duration
        let db = crate::db::open(db_path.to_str().unwrap()).await.unwrap();
        let keys = crypto::load_or_create(key_path.to_str().unwrap()).unwrap();

        let google_oidc = if identity_configured {
            Some(std::sync::Arc::new(
                oidc::GoogleOidc::discover(
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
        std::sync::Arc::new(crate::state::AppState {
            config: RwLock::new(crate::config::test_config()),
            wallet_status: RwLock::new(Vec::new()),
            armed: AtomicBool::new(false),
            bus: bus::new_bus(),
            control_tx,
            config_path: "unused-in-this-test".to_string(),
            api_token: "unused-in-this-test".to_string(),
            http_client: reqwest::Client::new(),
            identity_db: db,
            identity_cookie_key: keys.cookie_key,
            identity_totp_cipher: keys.totp_cipher,
            google_oidc,
            webauthn: None,
        })
    }

    /// Builds a request `Cookie:` header carrying `session_id`, encrypted
    /// with `state`'s real cookie key — the same round trip a real
    /// browser does (queue a cookie via `.add()`, read back the
    /// `Set-Cookie` the jar would have sent, and reuse just its
    /// `name=value` pair as what the NEXT request's `Cookie:` header
    /// would contain).
    fn session_cookie_header(state: &SharedState, session_id: &str) -> HeaderMap {
        let jar = PrivateCookieJar::from_headers(&HeaderMap::new(), state.identity_cookie_key.clone())
            .add(Cookie::new(SESSION_COOKIE, session_id.to_string()));
        let resp = jar.into_response();
        let set_cookie = resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("jar with an added cookie must produce a Set-Cookie header")
            .to_str()
            .unwrap();
        let name_value = set_cookie.split(';').next().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::COOKIE, name_value.parse().unwrap());
        headers
    }

    async fn make_user_and_session(state: &SharedState, admin_tier: bool) -> (String, String) {
        let user_id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users (id, google_sub, email, created_at) VALUES (?, ?, ?, 0)")
            .bind(&user_id)
            .bind(format!("sub-{user_id}"))
            .bind("stepup-test@example.com")
            .execute(&state.identity_db)
            .await
            .unwrap();
        let session_id = session::create_google_verified_session(&state.identity_db, &user_id).await.unwrap();
        if admin_tier {
            // Directly promote to admin_tier for test setup speed — the
            // real promotion path (mark_totp_verified + mark_webauthn_
            // verified) is already covered by totp.rs's and
            // webauthn.rs's own tests; what THIS test suite needs is a
            // session in each of the two possible states, not a second
            // copy of that promotion logic.
            sqlx::query("UPDATE sessions SET admin_tier = 1 WHERE id = ?")
                .bind(&session_id)
                .execute(&state.identity_db)
                .await
                .unwrap();
        }
        (user_id, session_id)
    }

    #[tokio::test]
    async fn identity_not_configured_bypasses_step_up_entirely() {
        let state = test_state(false).await;
        // No session cookie at all, no step-up header — still Ok(()),
        // because identity isn't configured for this instance. Matches
        // every other step 10 route's "opt-in, never breaks an existing
        // non-identity setup" convention.
        assert!(require_step_up(&state, &HeaderMap::new()).await.is_ok());
    }

    #[tokio::test]
    async fn no_session_is_401_not_a_silent_pass() {
        let state = test_state(true).await;
        let err = require_step_up(&state, &HeaderMap::new()).await.unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_admin_tier_session_is_403_distinct_from_401() {
        let state = test_state(true).await;
        let (_user_id, session_id) = make_user_and_session(&state, false).await;
        let headers = session_cookie_header(&state, &session_id);

        let err = require_step_up(&state, &headers).await.unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN, "a real-but-partial session must not be treated the same as no session");
    }

    #[tokio::test]
    async fn admin_tier_session_missing_step_up_header_is_428() {
        let state = test_state(true).await;
        let (_user_id, session_id) = make_user_and_session(&state, true).await;
        let headers = session_cookie_header(&state, &session_id);

        let err = require_step_up(&state, &headers).await.unwrap_err();
        assert_eq!(
            err.0,
            StatusCode::PRECONDITION_REQUIRED,
            "a fully-valid, old session must still be rejected without a FRESH code on this request"
        );
    }

    #[tokio::test]
    async fn admin_tier_session_wrong_code_is_412() {
        let state = test_state(true).await;
        let (_user_id, session_id) = make_user_and_session(&state, true).await;
        let mut headers = session_cookie_header(&state, &session_id);
        headers.insert("x-step-up-totp", "000000".parse().unwrap());

        let err = require_step_up(&state, &headers).await.unwrap_err();
        assert_eq!(err.0, StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn admin_tier_session_correct_fresh_code_succeeds_and_cannot_be_reused() {
        let state = test_state(true).await;
        let (user_id, session_id) = make_user_and_session(&state, true).await;

        // Enroll and enable a real TOTP secret for this user, exactly the
        // way 10d's setup flow does, so there's a real code to check
        // step-up against.
        let material = totp::start_setup(&state.identity_db, &state.identity_totp_cipher, &user_id, "stepup-test@example.com")
            .await
            .unwrap();
        let secret = totp_rs::Secret::try_from_base32(&material.secret_base32).unwrap();
        let live_totp = totp_rs::Builder::new()
            .with_algorithm(totp_rs::Algorithm::SHA1)
            .with_digits(6)
            .with_step_duration(30)
            .with_secret(secret.as_bytes().to_vec())
            .with_issuer(Some("mint-sniper"))
            .with_account_name("stepup-test@example.com")
            .build()
            .unwrap();
        let code = live_totp.generate_current().to_string();
        totp::verify_setup(&state.identity_db, &state.identity_totp_cipher, &user_id, "stepup-test@example.com", &code)
            .await
            .unwrap();

        // That same code already got consumed by verify_setup above —
        // step-up must reject reusing it (this IS the replay-protection
        // property, exercised through the actual HTTP-facing function,
        // not just totp.rs's own unit tests).
        let mut headers = session_cookie_header(&state, &session_id);
        headers.insert("x-step-up-totp", code.parse().unwrap());
        let err = require_step_up(&state, &headers).await.unwrap_err();
        assert_eq!(err.0, StatusCode::PRECONDITION_FAILED, "a code already consumed by setup must not be replayable as step-up");

        // A genuinely fresh next-step code succeeds.
        let next_code = live_totp.generate(crate::bus::now_ts() + 30).to_string();
        let mut headers = session_cookie_header(&state, &session_id);
        headers.insert("x-step-up-totp", next_code.parse().unwrap());
        assert!(require_step_up(&state, &headers).await.is_ok());
    }
}

#[cfg(test)]
mod cors_tests {
    //! Step 10.5b: the Cloudflare Tunnel hostname is added to the CORS
    //! allow-list by deriving it from `google_oauth_redirect_url`, the
    //! same value WebAuthn's rp_origin already uses (see
    //! `identity::webauthn::derive_origin`'s doc comment). Real assertions
    //! against real HTTP responses through the actual `router()` —
    //! not just "the code that builds the layer compiles" — same bar
    //! the rest of this codebase holds CORS/auth to.
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn minimal_state(google_oauth_redirect_url: &str) -> (SharedState, String) {
        use std::sync::atomic::AtomicBool;
        use tokio::sync::{mpsc, RwLock};
        let mut cfg = crate::config::test_config();
        cfg.google_oauth_redirect_url = google_oauth_redirect_url.to_string();
        let (control_tx, _control_rx) = mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("id.db");
        Box::leak(Box::new(dir));
        let state = std::sync::Arc::new(crate::state::AppState {
            config: RwLock::new(cfg),
            wallet_status: RwLock::new(Vec::new()),
            armed: AtomicBool::new(false),
            bus: bus::new_bus(),
            control_tx,
            config_path: "unused-in-this-test".to_string(),
            api_token: "test-token".to_string(),
            http_client: reqwest::Client::new(),
            identity_db: futures::executor::block_on(crate::db::open(db_path.to_str().unwrap())).unwrap(),
            identity_cookie_key: axum_extra::extract::cookie::Key::generate(),
            identity_totp_cipher: {
                use aes_gcm::{Aes256Gcm, KeyInit};
                Aes256Gcm::new_from_slice(&[0u8; 32]).unwrap()
            },
            google_oidc: None,
            webauthn: None,
        });
        (state, google_oauth_redirect_url.to_string())
    }

    async fn cors_response_for(state: SharedState, redirect_url: &str, request_origin: &str) -> Option<String> {
        let app = router(state, redirect_url);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/token")
                    .header("Origin", request_origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn public_hostname_is_echoed_back_when_configured() {
        let redirect_url = "https://sniper.example.com/auth/google/callback";
        let (state, _) = minimal_state(redirect_url);
        let allowed = cors_response_for(state, redirect_url, "https://sniper.example.com").await;
        assert_eq!(
            allowed.as_deref(),
            Some("https://sniper.example.com"),
            "an Origin matching the derived public hostname must be echoed back, no path/query included"
        );
    }

    #[tokio::test]
    async fn unrelated_origin_is_rejected_even_with_public_hostname_configured() {
        let redirect_url = "https://sniper.example.com/auth/google/callback";
        let (state, _) = minimal_state(redirect_url);
        let allowed = cors_response_for(state, redirect_url, "https://attacker.example").await;
        assert_eq!(allowed, None, "configuring one public origin must not turn CORS into an allow-any policy");
    }

    #[tokio::test]
    async fn no_public_hostname_configured_leaves_only_the_three_step_7b_origins() {
        let (state, _) = minimal_state("");
        let allowed = cors_response_for(state, "", "https://sniper.example.com").await;
        assert_eq!(
            allowed, None,
            "an unconfigured google_oauth_redirect_url must not silently allow an arbitrary origin"
        );
    }

    #[tokio::test]
    async fn existing_dev_origin_still_works_alongside_a_configured_public_hostname() {
        let redirect_url = "https://sniper.example.com/auth/google/callback";
        let (state, _) = minimal_state(redirect_url);
        let allowed = cors_response_for(state, redirect_url, "http://localhost:5173").await;
        assert_eq!(
            allowed.as_deref(),
            Some("http://localhost:5173"),
            "adding the public hostname must not have replaced or crowded out the existing step 7b origins"
        );
    }
}
