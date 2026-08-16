use crate::auth;
use crate::bus;
use crate::config::Config;
use crate::state::{ControlMsg, SharedState};
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
