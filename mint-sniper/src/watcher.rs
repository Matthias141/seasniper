use crate::bus::{self, EventBus};
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::TransactionRequest;
use anyhow::Result;
use futures::StreamExt;
use tokio::sync::watch;
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
