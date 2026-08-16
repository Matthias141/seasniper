use crate::bus::EventBus;
use crate::config::Config;
use alloy::primitives::{Address, U256};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

#[derive(Debug, Clone, serde::Serialize)]
pub struct WalletStatus {
    pub address: String,
    pub balance_eth: String,
    pub nonce: u64,
    pub healthy: bool,
}

/// Commands the UI can issue. Deliberately three states, not a generic
/// "run/stop" — Arm and FireNow are different actions with different risk:
/// Arm starts a *watcher* that will auto-fire on trigger, FireNow fires
/// immediately regardless of watcher state (the manual override for "the
/// project just tweeted mint is live, I don't trust the state-poll, go now").
pub enum ControlMsg {
    Arm,
    Disarm,
    FireNow,
    /// Internal-only: sign+cache every wallet's tx ahead of the actual
    /// trigger. Never sent from api.rs / the UI — control_loop sends this
    /// to itself (immediately for poll_state, after a lead-time sleep for
    /// timestamp mode) the same way it already self-sends FireNow when the
    /// watcher trips. Routing it through control_tx like everything else
    /// keeps wallets touched from exactly one place.
    Prepare,
    /// Fire an ad-hoc mint discovered by copymint.rs — carries its own
    /// (contract, calldata, value) rather than reusing control_loop's
    /// startup-configured mint target, since copymint targets a different,
    /// dynamically-discovered nftContract on every opportunity. Two, and
    /// only two, call sites ever send this: copymint.rs's watcher (only
    /// for a FREE opportunity with copymint_auto_fire_free enabled — see
    /// copymint.rs's `should_auto_fire`, which structurally cannot see a
    /// paid opportunity as fireable) and api.rs's `/api/copymint/fire`
    /// route (an authenticated manual action, gated by a human clicking a
    /// UI button, not by any config flag). Routed through control_tx like
    /// every other wallet-touching action, for the same single-writer
    /// reason as Arm/FireNow/Prepare.
    FireCopymint {
        contract: Address,
        calldata: Vec<u8>,
        value: U256,
    },
    /// Swaps the bot's active seadrop target (step 8b) — updates
    /// control_loop's runtime `admin_watch_target`/`mint_calldata`/
    /// `mint_value` for everything from here on. Sent only from
    /// `api.rs`'s `/api/target/set` route, after that route has already
    /// re-verified the new target fresh via `seadrop::fetch_public_drop`
    /// (never trusted from a client echo) and persisted it to
    /// `config.toml`. `nft_contract` is carried separately from
    /// `mint_calldata` because it's also needed to update
    /// `admin_watch_target` (mempool_watch's admin-tx address match) —
    /// the SeaDrop singleton address itself (`contract` in
    /// control_loop's scope) does not change on a target swap.
    ///
    /// SECURITY NOTE for step 10 (identity, in progress as of this
    /// writing): this changes where money goes on the next fire, the
    /// same sensitivity class as Arm/FireNow. Not step-up-auth-gated yet
    /// because step 10 isn't merged — this is a known TODO, not an
    /// oversight; see the route's own doc comment in api.rs.
    SetTarget {
        nft_contract: Address,
        mint_calldata: Vec<u8>,
        mint_value: U256,
    },
}

pub struct AppState {
    pub config: RwLock<Config>,
    pub wallet_status: RwLock<Vec<WalletStatus>>,
    pub armed: AtomicBool,
    pub bus: EventBus,
    pub control_tx: mpsc::Sender<ControlMsg>,
    pub config_path: String,
    /// Local bearer token — see auth.rs. Every route except GET /api/token
    /// itself requires this, as `Authorization: Bearer <token>` or a
    /// `?token=` query param (the WS upgrade route needs the latter;
    /// browsers can't set custom headers on a WebSocket handshake).
    pub api_token: String,
    /// Plain HTTPS REST client for OpenSea's API (step 8b/8c) — distinct
    /// from every `alloy::providers::Provider` elsewhere in this codebase,
    /// which all speak JSON-RPC to an EVM node, not a REST API. Built
    /// once in main() and reused (connection pooling), same reasoning as
    /// executor.rs's warmed RPC providers.
    pub http_client: reqwest::Client,
    /// Identity DB (step 10) — users/sessions/totp_secrets/webauthn_credentials.
    /// See db.rs's doc comment and CLAUDE.md's "Identity (step 10)" section.
    /// Not yet read or written anywhere as of 10b; wired into AppState now
    /// so the connection/migration path is exercised by every startup and
    /// test run from this point on, not just whenever 10c first touches it.
    pub identity_db: sqlx::SqlitePool,
    /// axum-extra `PrivateCookieJar` key — the session cookie (and the
    /// short-lived OIDC flow-id cookie) are encrypted+signed with this.
    /// See identity/crypto.rs.
    pub identity_cookie_key: axum_extra::extract::cookie::Key,
    /// AES-256-GCM cipher for TOTP secrets at rest. Same key file as
    /// `identity_cookie_key` (see identity/crypto.rs's doc comment for
    /// why one file, not two).
    pub identity_totp_cipher: aes_gcm::Aes256Gcm,
    /// `None` when Google Sign-In isn't configured (google_oauth_client_id
    /// unset in config.toml) — every /auth/google/* route must check this
    /// and return a clear "not configured" error rather than panic.
    pub google_oidc: Option<Arc<crate::identity::oidc::GoogleOidc>>,
    /// `None` under the exact same condition as `google_oidc` (WebAuthn's
    /// rp_id/rp_origin are derived FROM the Google redirect URL — see
    /// identity/webauthn.rs's doc comment) — every /auth/webauthn/* route
    /// must check this and return a clear "not configured" error.
    pub webauthn: Option<Arc<crate::identity::webauthn::WebauthnState>>,
}

// NOTE: deliberately no `impl FromRef<SharedState> for cookie::Key` here.
// SharedState is `Arc<AppState>` — Arc is a foreign type, so `Arc<AppState>`
// doesn't count as "local" for Rust's orphan rules even though AppState
// itself is local, and axum-extra's Key is also foreign. The usual
// axum-extra FromRef-based PrivateCookieJar extractor pattern needs the
// state type itself (not an Arc-wrapped one) to be local, which doesn't
// fit this codebase's existing Arc<AppState> convention (used everywhere
// else for cheap cloning into spawned tasks). Handlers that need the jar
// build it manually instead: `PrivateCookieJar::from_headers(&headers,
// state.identity_cookie_key.clone())` — see identity's usage in api.rs.

pub type SharedState = Arc<AppState>;
