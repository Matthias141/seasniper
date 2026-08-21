//! STEP 14a — push-based transaction-inclusion detection.
//!
//! Replaces `fire_prepared`'s old approach of awaiting
//! `PendingTransactionBuilder::get_receipt()` directly, whose internal
//! poll-loop interval — not real chain inclusion speed — is what step
//! 13f found was dominating the 7,551ms `dispatch_to_inclusion_ms`
//! measured on Robinhood Chain testnet's ~100ms blocks (see CLAUDE.md's
//! step 13/14 sections for the full before/after numbers).
//!
//! **NOT the same concern as `watcher::run_state_poll_watcher` (step
//! 13c).** That loop asks "has this drop's mint state flipped active,"
//! once per block, BEFORE anything is dispatched — the trigger-detection
//! phase. This module asks "has THIS specific, already-broadcast tx hash
//! been included," once per block, AFTER dispatch — the post-fire
//! confirmation phase. Different question, different timing, different
//! caller. Kept in a separate file specifically so a future read never
//! conflates the two — 13c's "don't throttle the per-block mint-state
//! check" reasoning says nothing about this module and this module says
//! nothing about that one.
//!
//! Two paths, in priority order:
//! - **PUSH** (primary): one shared WS subscription to new block headers
//!   per `fire_prepared` batch (not one per wallet — N wallets firing
//!   together share a single `establish_block_ticker` call, avoiding N
//!   redundant WS connections), reusing the exact `subscribe_blocks()`
//!   call `watcher.rs` already uses. On every new head, check
//!   `eth_getTransactionReceipt` for the wallet's own hash. Detection
//!   latency is tied to real block arrival, not an arbitrary interval.
//! - **POLL** (fallback): if the WS subscription can't be established,
//!   fall back to HTTP polling at `Config::block_time_ms` — sized to the
//!   configured chain's real expected block time, not a fixed interval
//!   that only happens to suit mainnet's ~12s blocks.
//!
//! Both paths are bounded by `Config::inclusion_timeout_ms` — a tx that
//! never gets included reports `TimedOut`, distinct from both a
//! confirmed success and a confirmed on-chain revert, rather than
//! hanging the caller forever.
//!
//! **Expected, not a defect, if the PUSH path never actually engages in
//! this specific sandbox:** `establish_block_ticker` depends on the same
//! WS transport `run_state_poll_watcher`/`run_mempool_watcher` already
//! use, which gap #11 established fails here with `invalid peer
//! certificate: UnknownIssuer` (alloy's WS stack is hard-compiled
//! against `webpki-roots`, incompatible with this sandbox's
//! TLS-intercepting proxy) — reconfirmed on Robinhood Chain testnet in
//! step 13d. This module is built and unit-tested here; live
//! verification that the PUSH path itself engages (rather than always
//! falling back to POLL) needs an environment without that limitation.
//! The POLL fallback path IS the one step 13d/14b's live dry runs and
//! benchmark actually exercised end to end.

use crate::executor::HttpProvider;
use alloy::primitives::TxHash;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::TransactionReceipt;
use anyhow::Context;
use std::time::Duration;
use tokio::sync::watch;
use tracing::warn;

/// One shared "a new block just arrived" signal for an entire
/// `fire_prepared` batch. `watch::Receiver` is `Clone` — every wallet's
/// own `wait_for_receipt` call gets its own clone of the same
/// underlying channel, so establishing this once covers however many
/// wallets are firing together. The carried value (a monotonically
/// increasing tick count) is never read for its own sake — only
/// `changed()` matters — a `u64` counter is just something cheap and
/// `Copy` to send.
pub type BlockTicker = watch::Receiver<u64>;

/// Attempts to open a WS subscription to new block headers and forwards
/// each arrival over a `watch::channel`. Returns `None` on ANY failure
/// (bad URL, connect failure, subscribe failure, or a 5s connect
/// timeout) rather than propagating an error — the caller's fallback IS
/// the designed response to `None`, not an exceptional case to handle
/// separately.
///
/// The 5s connect timeout exists because a hanging TLS handshake against
/// some OTHER network's dead/unreachable endpoint could plausibly hang
/// rather than fail cleanly the way this sandbox's own TLS-interception
/// limitation does (see this module's doc comment) — real protection
/// for a real class of failure, not defensive-for-its-own-sake.
/// `ws_url` must be an *eth* WebSocket RPC (`subscribe_blocks` / newHeads).
/// Alchemy (or any third-party) PUSH on this socket means the receipt was
/// seen by this RPC, not that the sequencer has included the tx. Do not
/// pass the Robinhood Nitro feed (`wss://feed.*.chain.robinhood.com`) —
/// that is not an eth WS.
pub async fn establish_block_ticker(ws_url: &str) -> Option<BlockTicker> {
    let ws_url = ws_url.to_string();
    // STEP 17b — this path's whole design is "fail quietly, HTTP polling
    // is the intended response" (see this fn's doc comment), so unlike
    // the other three `connect_ws` call sites this one still collapses
    // to `None` rather than propagating an `Err`. But collapsing used to
    // throw the underlying reason away entirely — the operator saw only
    // "WS subscription unavailable," never *why* (auth error vs. DNS
    // failure vs. TLS failure vs. connection refused all looked
    // identical). Capture it via anyhow::Context and log it before
    // discarding, same "surface the real reason, don't swallow it"
    // standard as step 3b's revert-reason fix.
    let redacted = crate::config::redact_rpc_url(&ws_url);
    let connect = async {
        let ws = WsConnect::new(ws_url.clone());
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_ws(ws)
            .await
            .with_context(|| {
                format!("inclusion: opening the WebSocket RPC connection to {redacted}")
            })?;
        provider
            .subscribe_blocks()
            .await
            .context("inclusion: subscribing to new block headers")
    };

    let sub = match tokio::time::timeout(Duration::from_secs(5), connect).await {
        Ok(Ok(sub)) => sub,
        Ok(Err(e)) => {
            warn!(
                "inclusion detection: WS subscription unavailable ({e:#}), falling back to HTTP polling"
            );
            return None;
        }
        Err(_) => {
            warn!(
                "inclusion detection: WS connect to {redacted} timed out after 5s, falling back to HTTP polling"
            );
            return None;
        }
    };

    let (tx, rx) = watch::channel(0u64);
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut stream = sub.into_stream();
        let mut tick: u64 = 0;
        while stream.next().await.is_some() {
            tick += 1;
            if tx.send(tick).is_err() {
                break; // every receiver dropped — this fire's wallets are all done
            }
        }
    });
    Some(rx)
}

/// Distinct from both a confirmed success and a confirmed on-chain
/// revert (both of which are `Included` here — `fire_prepared` decides
/// success/revert from `receipt.status()` same as before 14a; this
/// module only ever answers "did a receipt show up in time").
pub enum InclusionOutcome {
    // Boxed: TransactionReceipt is large (~592 bytes) and this enum's
    // other variant is tiny — clippy's large_enum_variant under CI's
    // `-D warnings` (confirmed live, not assumed) flags the unboxed
    // version, since every InclusionOutcome would otherwise pay
    // TimedOut's stack cost for the rare-by-comparison Included case.
    Included { receipt: Box<TransactionReceipt>, method: &'static str },
    TimedOut { method: &'static str },
}

/// Waits for `tx_hash`'s receipt via whichever path is available.
/// `ticker: Some(_)` selects PUSH (checks on every new block tick, plus
/// once immediately in case inclusion already happened before this call
/// started waiting); `ticker: None` selects POLL, at `poll_interval`
/// (the caller derives this from `Config::block_time_ms` — see
/// `fire_prepared`).
pub async fn wait_for_receipt(
    provider: &HttpProvider,
    tx_hash: TxHash,
    ticker: Option<BlockTicker>,
    poll_interval: Duration,
    overall_timeout: Duration,
) -> InclusionOutcome {
    let method = if ticker.is_some() { "push" } else { "poll" };

    let attempt = async {
        match ticker {
            Some(mut ticker) => loop {
                if let Ok(Some(receipt)) = provider.get_transaction_receipt(tx_hash).await {
                    return receipt;
                }
                if ticker.changed().await.is_err() {
                    // The ticker task exited (its Sender dropped, e.g. the
                    // WS connection itself died mid-fire) — no more
                    // block-arrival pushes will ever come. Degrade to a
                    // plain poll loop for the remainder of the overall
                    // timeout rather than waiting on a signal that will
                    // never fire again.
                    loop {
                        tokio::time::sleep(poll_interval).await;
                        if let Ok(Some(receipt)) = provider.get_transaction_receipt(tx_hash).await {
                            return receipt;
                        }
                    }
                }
            },
            None => loop {
                if let Ok(Some(receipt)) = provider.get_transaction_receipt(tx_hash).await {
                    return receipt;
                }
                tokio::time::sleep(poll_interval).await;
            },
        }
    };

    match tokio::time::timeout(overall_timeout, attempt).await {
        Ok(receipt) => InclusionOutcome::Included { receipt: Box::new(receipt), method },
        Err(_) => InclusionOutcome::TimedOut { method },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `establish_block_ticker` against an unreachable/invalid URL must
    /// return `None` promptly (well under its own 5s ceiling), not hang
    /// or panic — the exact shape `fire_prepared` depends on to always
    /// have a usable fallback.
    #[tokio::test]
    async fn establish_block_ticker_returns_none_for_an_invalid_url() {
        let start = std::time::Instant::now();
        let result = establish_block_ticker("not a url at all").await;
        assert!(result.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "an obviously-invalid URL must fail fast, not wait anywhere near the 5s connect ceiling"
        );
    }

    /// A tick count on a `watch::channel` is a real, working Clone-able
    /// signal — proven directly rather than assumed from `watch`'s own
    /// docs, since `wait_for_receipt`'s whole PUSH path depends on
    /// `changed()` actually unblocking when `send()` is called from a
    /// different task.
    #[tokio::test]
    async fn block_ticker_changed_unblocks_on_send() {
        let (tx, mut rx) = watch::channel(0u64);
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(1);
        });
        tokio::time::timeout(Duration::from_secs(1), rx.changed())
            .await
            .expect("changed() must unblock once send() is called")
            .expect("channel must still be open");
        sender.await.unwrap();
    }
}
