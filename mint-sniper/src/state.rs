use crate::bus::EventBus;
use crate::config::Config;
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
}

pub type SharedState = Arc<AppState>;
