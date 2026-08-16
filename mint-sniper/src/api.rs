use crate::auth;
use crate::bus;
use crate::config::Config;
use crate::copymint;
use crate::state::{ControlMsg, SharedState};
use crate::target;
use alloy::primitives::Address;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderValue, Method, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::atomic::Ordering;
use tower_http::cors::CorsLayer;

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
/// place. CORS is an explicit allow-list, not `Any` — an arm/fire-capable
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
        .route("/ws/events", get(ws_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_token));

    let public = Router::new().route("/api/token", get(get_token));

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
