use crate::bus::{self, EventBus};
use crate::state::ControlMsg;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::{TransactionRequest, TransactionTrait};
use anyhow::Result;
use futures::StreamExt;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

/// Fires exactly once (watch channel flips to `true`) the moment the mint
/// condition is satisfied. Kept dead simple on purpose: complexity here is
/// latency you're paying at the worst possible moment.
pub async fn run_state_poll_watcher(
    ws_url: String,
    contract: Address,
    mint_state_selector: Vec<u8>,
    trigger_tx: watch::Sender<bool>,
    event_bus: EventBus,
) -> Result<()> {
    let ws = WsConnect::new(ws_url);
    let provider = ProviderBuilder::new().on_ws(ws).await?;

    let mut stream = provider.subscribe_blocks().await?.into_stream();
    info!("watcher: subscribed to newHeads, polling mint state each block");
    bus::log(&event_bus, "info", "watcher armed — subscribed to new blocks");

    while let Some(_header) = stream.next().await {
        let call = TransactionRequest::default()
            .to(contract)
            .input(mint_state_selector.clone().into());

        match provider.call(&call).await {
            Ok(result) => {
                // mintActive() -> bool ABI-encodes as a single 32-byte word,
                // last byte 0x01 == true. Adjust decoding if the view fn
                // returns a different type (e.g. a struct with saleStart).
                let is_active = result.last().map(|b| *b == 1).unwrap_or(false);
                if is_active {
                    info!("TRIGGER: mint state flipped active");
                    bus::log(&event_bus, "warn", "TRIGGER: mint state flipped active");
                    let _ = trigger_tx.send(true);
                    break;
                }
            }
            Err(e) => warn!(error = %e, "state check call failed, retrying next block"),
        }
    }
    Ok(())
}

/// Watches the mempool for a pending transaction FROM the configured admin
/// address TO the watched contract, and fires the instant it's SEEN
/// pending — before it's even confirmed in a block, which is the entire
/// point of this mode versus `run_state_poll_watcher`'s confirmed-block
/// check.
///
/// SCOPE NOTE — matches on (from, to) only, not on decoding which function
/// is being called. In seadrop mode, the on-chain call that actually
/// flips a drop live (`SeaDrop.updatePublicDrop`) is made BY the nft
/// contract itself — `msg.sender` is required to be the nft contract via
/// SeaDrop.sol's `onlyINonFungibleSeaDropToken` modifier, it is not called
/// directly by a human admin — so the transaction actually visible in the
/// mempool is the admin's call to *their nft contract's own* admin-only
/// wrapper (whatever that's named on their specific token contract), never
/// a transaction whose `to` is the SeaDrop singleton. `watch_target` is
/// therefore the nft contract in seadrop mode, not the singleton (see
/// main.rs's `admin_watch_target` computation). Matching only on the
/// address pair, not also requiring a specific selector, is a deliberate
/// simplification: in custom mode there's no way to know a project's
/// "enable" function signature ahead of time, and in seadrop mode the
/// admin wrapper's exact selector isn't guaranteed to be the same across
/// different SeaDrop-token base contracts either. The cost is that ANY tx
/// from admin to watch_target fires the trigger, not just the specific one
/// that flips the drop live — acceptable for a wallet that's presumably
/// only sending drop-related transactions to that contract around the
/// mint window, not a general concern.
///
/// REQUIRES a full pending-transaction subscription
/// (`eth_subscribe("newPendingTransactions", true)`), which alloy exposes
/// as `Provider::subscribe_full_pending_transactions` — needs (a) a
/// WebSocket RPC, since this is a pubsub-only method, and (b) a node that
/// actually exposes full mempool visibility, which per alloy's own docs
/// requires Geth 1.11+ and which many public/free RPC providers disable
/// entirely (mempool visibility is valuable and commonly rate-limited or
/// turned off even over WS). If the subscription itself fails, this does
/// NOT sit there "armed" while watching nothing: it logs a loud error,
/// disarms via `control_tx`, and returns `Err` so the failure is visible
/// immediately rather than a control panel that says ARMED indefinitely.
pub async fn run_mempool_watcher(
    ws_url: String,
    admin: Address,
    watch_target: Address,
    trigger_tx: watch::Sender<bool>,
    event_bus: EventBus,
    control_tx: mpsc::Sender<ControlMsg>,
) -> Result<()> {
    let ws = WsConnect::new(ws_url);
    let provider = ProviderBuilder::new().on_ws(ws).await?;

    let sub = match provider.subscribe_full_pending_transactions().await {
        Ok(sub) => sub,
        Err(e) => {
            let msg = format!(
                "mempool_watch failed to subscribe to full pending transactions ({e}). \
                 This mode needs a WebSocket RPC on a node that exposes full mempool \
                 visibility (Geth 1.11+, newPendingTransactions with full-tx bodies \
                 enabled) — many public/free RPC endpoints don't support this even over \
                 WS. Disarming rather than staying armed while watching nothing; switch \
                 ws_rpc_url to a provider with mempool access, or use trigger_mode = \
                 \"poll_state\" or \"timestamp\" instead."
            );
            bus::log(&event_bus, "error", msg.clone());
            let _ = control_tx.send(ControlMsg::Disarm).await;
            anyhow::bail!(msg);
        }
    };

    info!(%admin, %watch_target, "watcher: subscribed to full pending transactions");
    bus::log(
        &event_bus,
        "info",
        format!("mempool watcher armed — watching for pending tx from {admin:#x} to {watch_target:#x}"),
    );

    let mut stream = sub.into_stream();
    while let Some(tx) = stream.next().await {
        if tx.from == admin && tx.to() == Some(watch_target) {
            let tx_hash = *tx.inner.tx_hash();
            info!(%tx_hash, "TRIGGER: admin tx seen pending (unconfirmed)");
            bus::log(
                &event_bus,
                "warn",
                format!("TRIGGER: pending tx {tx_hash:#x} from admin to {watch_target:#x} seen — firing"),
            );
            let _ = trigger_tx.send(true);
            break;
        }
    }
    Ok(())
}

/// Simpler alternative trigger: fire at a known unix timestamp rather than
/// polling a view function. Use when the project has announced an exact
/// mint time and gates by block.timestamp internally.
pub async fn run_timestamp_watcher(
    target_unix: u64,
    trigger_tx: watch::Sender<bool>,
) -> Result<()> {
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        if now >= target_unix {
            info!("TRIGGER: target timestamp reached");
            let _ = trigger_tx.send(true);
            return Ok(());
        }
        // Tighten polling interval as the deadline approaches.
        let remaining = target_unix.saturating_sub(now);
        let sleep_ms = if remaining > 5 { 500 } else { 20 };
        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
    }
}
