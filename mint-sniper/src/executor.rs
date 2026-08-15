use crate::bus::{EventBus, ServerEvent};
use crate::config::Config;
use crate::wallet::{wallet_to_eth_wallet, ManagedWallet};
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use anyhow::Result;
use futures::future::join_all;
use rand::Rng;
use tracing::{error, info};

/// Fires one mint tx per wallet, each broadcast concurrently to every
/// configured HTTP RPC endpoint (first confirmation wins, rest are wasted
/// but harmless — same signed tx, same hash).
///
/// Jitter is intentional: identical gas price + identical block from N
/// wallets is a trivial sybil-clustering signature. A few hundred ms and a
/// few % of gas spread costs you almost nothing in a free mint and buys
/// meaningfully better odds of surviving allowlist-eligibility heuristics
/// on future drops from the same project.
pub async fn fire_all_wallets(
    cfg: &Config,
    wallets: &mut [ManagedWallet],
    contract: Address,
    calldata: Vec<u8>,
    mint_value_per_wallet: U256,
    bus: &EventBus,
) -> Result<()> {
    let _ = bus.send(ServerEvent::TriggerFired { manual: false });
    let provider = ProviderBuilder::new().on_http(cfg.http_rpc_urls[0].parse()?);
    let base_fee = provider.get_gas_price().await?; // legacy fallback if EIP-1559 fee history call fails
    let priority_fee_wei = ((base_fee as f64) * cfg.priority_fee_multiplier) as u128;
    let cap_wei = (cfg.max_priority_fee_gwei_cap * 1e9) as u128;
    let priority_fee_wei = priority_fee_wei.min(cap_wei);

    let mut handles = Vec::with_capacity(wallets.len());

    for w in wallets.iter_mut() {
        let calldata = calldata.clone();
        let http_urls = cfg.http_rpc_urls.clone();
        let eth_wallet: EthereumWallet = wallet_to_eth_wallet(w);
        let nonce = w.next_nonce;
        w.next_nonce += 1; // reserve immediately, don't wait on the send result

        let jitter_ms: u64 = rand::thread_rng()
            .gen_range(cfg.jitter_ms_min..=cfg.jitter_ms_max);
        let gas_jitter_pct: i64 = rand::thread_rng()
            .gen_range(-(cfg.gas_jitter_pct as i64)..=(cfg.gas_jitter_pct as i64));
        let wallet_priority_fee = apply_pct_jitter(priority_fee_wei, gas_jitter_pct);

        let address = w.address;
        let contract_addr = contract;
        let bus = bus.clone();

        handles.push(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;

            let tx = TransactionRequest::default()
                .to(contract_addr)
                .input(calldata.into())
                .nonce(nonce)
                .max_priority_fee_per_gas(wallet_priority_fee)
                .value(mint_value_per_wallet); // 0 for free mints; SeaDrop-mode public-stage price otherwise

            // Race the same signed tx across every RPC; first to land wins,
            // duplicates just get dropped as already-known by the mempool.
            let sends = http_urls.iter().map(|url| {
                let tx = tx.clone();
                let eth_wallet = eth_wallet.clone();
                let url = url.clone();
                async move {
                    // Explicit `anyhow::Result` here (rather than inferring from
                    // `.watch()`'s own error type) because this block also has to
                    // convert `url::ParseError` from the RPC URL parse below — the
                    // two error types don't otherwise unify.
                    let parsed_url = url
                        .parse()
                        .map_err(|e| anyhow::anyhow!("invalid RPC url {url}: {e}"))?;
                    let provider = ProviderBuilder::new().wallet(eth_wallet).on_http(parsed_url);
                    let tx_hash = provider.send_transaction(tx).await?.watch().await?;
                    Ok::<_, anyhow::Error>(tx_hash)
                }
            });

            match futures::future::select_ok(sends.map(Box::pin)).await {
                Ok((tx_hash, _)) => {
                    info!(%address, %tx_hash, "mint confirmed");
                    let _ = bus.send(ServerEvent::MintResult {
                        address: format!("{address:#x}"),
                        success: true,
                        detail: format!("{tx_hash:#x}"),
                    });
                }
                Err(e) => {
                    error!(%address, error = %e, "all RPC broadcasts failed for this wallet");
                    let _ = bus.send(ServerEvent::MintResult {
                        address: format!("{address:#x}"),
                        success: false,
                        detail: e.to_string(),
                    });
                }
            }
        }));
    }

    join_all(handles).await;
    Ok(())
}

fn apply_pct_jitter(value: u128, pct: i64) -> u128 {
    let delta = (value as i128 * pct as i128) / 100;
    (value as i128 + delta).max(0) as u128
}
