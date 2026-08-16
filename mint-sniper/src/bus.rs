use serde::Serialize;
use tokio::sync::broadcast;

/// Every state change the UI needs to know about, in one typed enum.
/// Serialized straight to JSON and pushed over /ws/events. Keeping this as
/// a flat tagged enum (not a generic "log string") is what lets the UI
/// render wallet rows, RPC health dots, and the log feed from the *same*
/// stream instead of polling three separate endpoints.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Log {
        level: String, // "info" | "warn" | "error"
        message: String,
        ts: u64,
    },
    ArmedState {
        armed: bool,
    },
    WalletUpdate {
        address: String,
        balance_eth: String,
        nonce: u64,
        healthy: bool,
    },
    RpcHealth {
        url: String,
        healthy: bool,
        latency_ms: u64,
    },
    TriggerFired {
        manual: bool,
    },
    MintResult {
        address: String,
        success: bool,
        detail: String,
    },
    /// Emitted by `api::put_config` after a validated config write. Kept as
    /// its own typed variant, not folded into `Log`, specifically so
    /// `audit.rs`'s bus subscriber can persist config changes without also
    /// persisting every routine `Log` message (RPC errors, info noise) —
    /// see `audit.rs`'s doc comment.
    ConfigChanged,
}

pub type EventBus = broadcast::Sender<ServerEvent>;

/// Buffer of 256: a client that's mid-reconnect during a fire sequence can
/// miss events (broadcast channels drop oldest on lag, they don't block).
/// That's an acceptable trade for a control-panel — the /api/status
/// endpoint is the source of truth for current state; the WS stream is
/// for real-time feel, not guaranteed delivery.
pub fn new_bus() -> EventBus {
    let (tx, _rx) = broadcast::channel(256);
    tx
}

pub fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn log(bus: &EventBus, level: &str, message: impl Into<String>) {
    let _ = bus.send(ServerEvent::Log {
        level: level.to_string(),
        message: message.into(),
        ts: now_ts(),
    });
}
