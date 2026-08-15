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
}

pub struct AppState {
    pub config: RwLock<Config>,
    pub wallet_status: RwLock<Vec<WalletStatus>>,
    pub armed: AtomicBool,
    pub bus: EventBus,
    pub control_tx: mpsc::Sender<ControlMsg>,
    pub config_path: String,
}

pub type SharedState = Arc<AppState>;
