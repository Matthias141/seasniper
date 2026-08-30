//! Delegated mint mode (v1) — the actual serial-fire loop.
//!
//! **This is `DELEGATED_SERIAL`, not a batch.** One operator wallet sends
//! N sequential `mintPublic` transactions, one nonce and one broadcast
//! per receiver, awaited to a real receipt before moving to the next.
//! This is NOT as fast as a true batched helper-contract mint (N
//! receivers in one tx / one nonce) and must never be logged, labeled, or
//! described anywhere as "delegated batch" or "one transaction" — every
//! event this module emits carries the literal string `DELEGATED_SERIAL`
//! for exactly this reason. No helper/factory contract exists in v1 (see
//! this crate's `delegated/mod.rs` doc comment) — that is a real,
//! separate design decision, not something this file works around.
//!
//! Deliberately self-contained: does not call `executor::prepare_fire`/
//! `fire_prepared`, does not touch `wallet.rs`'s `ManagedWallet`s, and
//! shares no mutable state with the parallel-EOA path. The operator
//! signer used here is always freshly derived from the configured
//! mnemonic at the moment of firing (`wallet_derivation::
//! derive_operator_and_receivers`) — never cached across calls, never
//! held in `AppState`.

use crate::bus::{self, EventBus, ServerEvent};
use crate::config::Config;
use crate::delegated::{preflight, wallet_derivation};
use crate::seadrop;
use alloy::eips::eip2718::Encodable2718;
use alloy::network::{EthereumWallet, NetworkTransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use anyhow::{Context, Result};

/// Runs one full delegated mint attempt: derives operator + receivers
/// fresh from the configured mnemonic, runs preflight (see
/// `preflight::run_preflight`'s own doc comment for exactly how a
/// `minterIfNotPayer` mismatch is distinguished from any other failure —
/// **refuses to fire at all on `MinterMismatch`, never silently falls
/// back to the parallel path**), then fires sequentially, one receiver at
/// a time, waiting for a real receipt before advancing.
///
/// **Nonce safety on a mid-run failure, stated precisely:** the
/// operator's nonce is only ever advanced after a receipt is actually
/// observed (success OR on-chain revert both consume the nonce slot —
/// both advance it). If a broadcast itself fails (e.g. `send_raw_
/// transaction` errors outright — insufficient funds, RPC rejection),
/// the nonce was never consumed on-chain, so it is NOT advanced, and the
/// loop continues to the next receiver (a transient per-attempt error
/// doesn't need to halt the whole run). If a receipt cannot be confirmed
/// within the timeout, whether the nonce was actually consumed is
/// genuinely ambiguous — **the loop stops immediately** rather than
/// guessing, since blindly continuing risks either a nonce collision (if
/// it wasn't consumed) or silently skipping receivers (if it was).
pub async fn run_delegated_mint(cfg: &Config, bus: &EventBus) -> Result<()> {
    if cfg.delegate_mnemonic_env.is_empty() {
        anyhow::bail!("delegate_mnemonic_env is not set — required for mint_execution = \"delegated\"");
    }
    let mnemonic = std::env::var(&cfg.delegate_mnemonic_env)
        .with_context(|| format!("env var {} not set (delegate_mnemonic_env)", cfg.delegate_mnemonic_env))?;

    let (operator, receivers) =
        wallet_derivation::derive_operator_and_receivers(mnemonic, cfg.delegate_count)
            .context("deriving operator + receivers from OPERATOR_MNEMONIC")?;

    let seadrop_address: Address = if cfg.seadrop_address.is_empty() {
        seadrop::SEADROP_1_0_MAINNET
            .parse()
            .context("hardcoded SeaDrop mainnet address failed to parse (should never happen)")?
    } else {
        cfg.seadrop_address.parse().context("bad seadrop_address in config")?
    };
    let nft_contract: Address = cfg
        .nft_contract
        .parse()
        .context("bad nft_contract (required for mint_execution = \"delegated\")")?;
    let fee_recipient: Address = cfg
        .fee_recipient
        .parse()
        .context("bad fee_recipient (required for mint_execution = \"delegated\")")?;

    let http_rpc_url = cfg
        .http_rpc_urls
        .first()
        .context("http_rpc_urls is empty — at least one RPC is required")?;

    let first_receiver = *receivers
        .addresses()
        .first()
        .context("delegate_count derived zero receivers — should be unreachable, validate() requires >= 1")?;

    // STEP: preflight, run fresh right here, immediately before firing —
    // never trusted from an earlier /api/delegated/preflight call, which
    // could be stale by the time an operator actually confirms. Same
    // "always re-verify fresh before an action with real consequences"
    // principle target.rs's /api/target/set already applies.
    let outcome = preflight::run_preflight(
        http_rpc_url,
        seadrop_address,
        nft_contract,
        fee_recipient,
        operator.address(),
        first_receiver,
        cfg.quantity_per_wallet,
        cfg.delegate_count,
    )
    .await
    .context("delegated mint preflight failed to run")?;

    let estimated_max_spend_wei = match outcome {
        preflight::PreflightOutcome::Ok { estimated_max_spend_wei } => estimated_max_spend_wei,
        preflight::PreflightOutcome::MinterMismatch { revert_reason } => {
            let msg = format!(
                "DELEGATED_SERIAL refused to arm: this contract rejects a nonzero \
                 minterIfNotPayer — {revert_reason}"
            );
            bus::log(bus, "error", msg.clone());
            anyhow::bail!("{msg}");
        }
        preflight::PreflightOutcome::OtherFailure { revert_reason } => {
            let msg = format!("DELEGATED_SERIAL preflight failed: {revert_reason}");
            bus::log(bus, "error", msg.clone());
            anyhow::bail!("{msg}");
        }
    };

    let _ = bus.send(ServerEvent::DelegatedRunStarted {
        delegate_count: cfg.delegate_count,
        estimated_max_spend_wei: estimated_max_spend_wei.to_string(),
    });

    let drop = seadrop::fetch_public_drop(http_rpc_url, seadrop_address, nft_contract)
        .await
        .context("getPublicDrop failed")?;
    let value_per_call = drop.mint_price_wei.saturating_mul(U256::from(cfg.quantity_per_wallet));

    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(http_rpc_url.parse()?);
    let chain_id = provider.get_chain_id().await.context("fetching chain id")?;
    let mut nonce = provider
        .get_transaction_count(operator.address())
        .await
        .context("fetching operator nonce")?;
    let eth_wallet = EthereumWallet::from(operator.clone());

    let mut minted: u32 = 0;
    let mut attempted: u32 = 0;
    let mut total_cost_wei = U256::ZERO;

    'firing: for (i, receiver) in receivers.addresses().iter().enumerate() {
        attempted += 1;
        let receiver_index = (i + 1) as u32;
        let receiver_str = format!("{receiver:#x}");

        let calldata = match preflight::encode_mint_public_delegated(
            nft_contract,
            fee_recipient,
            *receiver,
            cfg.quantity_per_wallet,
        ) {
            Ok(c) => c,
            Err(e) => {
                let _ = bus.send(ServerEvent::DelegatedMintResult {
                    receiver_index,
                    receiver_address: receiver_str,
                    success: false,
                    detail: format!("DELEGATED_SERIAL: building calldata failed: {e:#}"),
                });
                continue;
            }
        };

        let base_fee = match provider.get_gas_price().await {
            Ok(v) => v,
            Err(e) => {
                let _ = bus.send(ServerEvent::DelegatedMintResult {
                    receiver_index,
                    receiver_address: receiver_str,
                    success: false,
                    detail: format!("DELEGATED_SERIAL: fetching gas price failed: {e:#}"),
                });
                continue;
            }
        };
        let priority_fee_wei = (((base_fee as f64) * cfg.priority_fee_multiplier) as u128)
            .min((cfg.max_priority_fee_gwei_cap * 1e9) as u128);
        let max_fee_per_gas = priority_fee_wei + base_fee.saturating_mul(2);

        let mut probe_tx = TransactionRequest::default()
            .to(seadrop_address)
            .from(operator.address())
            .input(calldata.clone().into())
            .value(value_per_call);
        let estimated_gas = match provider.estimate_gas(probe_tx.clone()).await {
            Ok(g) => g,
            Err(e) => {
                let _ = bus.send(ServerEvent::DelegatedMintResult {
                    receiver_index,
                    receiver_address: receiver_str,
                    success: false,
                    detail: format!("DELEGATED_SERIAL: gas estimation failed: {e:#}"),
                });
                continue;
            }
        };
        let gas_limit = estimated_gas + (estimated_gas * cfg.gas_limit_headroom_pct) / 100;

        probe_tx = probe_tx
            .nonce(nonce)
            .gas_limit(gas_limit)
            .max_fee_per_gas(max_fee_per_gas)
            .max_priority_fee_per_gas(priority_fee_wei);
        let mut tx = probe_tx;
        tx.chain_id = Some(chain_id);

        let envelope = match tx.build(&eth_wallet).await {
            Ok(e) => e,
            Err(e) => {
                let _ = bus.send(ServerEvent::DelegatedMintResult {
                    receiver_index,
                    receiver_address: receiver_str,
                    success: false,
                    detail: format!("DELEGATED_SERIAL: signing failed: {e:#}"),
                });
                continue;
            }
        };
        let raw_tx = envelope.encoded_2718();
        let expected_hash = *envelope.tx_hash();

        if let Err(e) = provider.send_raw_transaction(&raw_tx).await {
            // Broadcast itself failed — the nonce was never consumed
            // on-chain, so it is deliberately NOT advanced here (the
            // next receiver's attempt reuses the same nonce value).
            let _ = bus.send(ServerEvent::DelegatedMintResult {
                receiver_index,
                receiver_address: receiver_str,
                success: false,
                detail: format!("DELEGATED_SERIAL: broadcast failed: {e:#}"),
            });
            continue;
        }

        let receipt_timeout = std::time::Duration::from_millis(cfg.inclusion_timeout_ms);
        let receipt_result = tokio::time::timeout(receipt_timeout, async {
            loop {
                if let Ok(Some(r)) = provider.get_transaction_receipt(expected_hash).await {
                    return r;
                }
                tokio::time::sleep(std::time::Duration::from_millis(cfg.block_time_ms)).await;
            }
        })
        .await;

        match receipt_result {
            Ok(receipt) => {
                // A real receipt was observed — the nonce IS consumed on
                // -chain either way (success or revert), so it advances
                // regardless of receipt.status().
                nonce += 1;
                if receipt.status() {
                    minted += 1;
                    total_cost_wei = total_cost_wei.saturating_add(value_per_call).saturating_add(
                        U256::from(receipt.gas_used).saturating_mul(U256::from(base_fee)),
                    );
                    let _ = bus.send(ServerEvent::DelegatedMintResult {
                        receiver_index,
                        receiver_address: receiver_str,
                        success: true,
                        detail: format!("{expected_hash:#x}"),
                    });
                } else {
                    let _ = bus.send(ServerEvent::DelegatedMintResult {
                        receiver_index,
                        receiver_address: receiver_str,
                        success: false,
                        detail: format!(
                            "DELEGATED_SERIAL: reverted on-chain — tx {expected_hash:#x}, block {}, gas used {}",
                            receipt.block_number.map_or("?".to_string(), |b| b.to_string()),
                            receipt.gas_used
                        ),
                    });
                }
            }
            Err(_) => {
                // Timed out — nonce state is genuinely ambiguous. Stop the
                // whole run rather than guess (see this function's own
                // doc comment).
                let _ = bus.send(ServerEvent::DelegatedMintResult {
                    receiver_index,
                    receiver_address: receiver_str,
                    success: false,
                    detail: format!(
                        "DELEGATED_SERIAL: receipt not confirmed within {}ms — tx {expected_hash:#x} may \
                         still be pending; nonce state is now ambiguous, stopping this run rather than \
                         risking a nonce collision. Check a block explorer before re-arming.",
                        cfg.inclusion_timeout_ms
                    ),
                });
                break 'firing;
            }
        }
    }

    let _ = bus.send(ServerEvent::DelegatedRunComplete {
        minted,
        attempted,
        total_cost_wei: total_cost_wei.to_string(),
    });

    Ok(())
}
