use crate::bus::{self, EventBus, ServerEvent};
use crate::config::Config;
use crate::wallet::{wallet_to_eth_wallet, ManagedWallet};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::{EthereumWallet, NetworkTransactionBuilder};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use anyhow::{Context, Result};
use futures::future::join_all;
use rand::Rng;
use tracing::{error, info};

/// A plain HTTP JSON-RPC provider with no wallet/filler layers attached —
/// used for both connection warming and, after `prepare_fire`, raw-tx
/// broadcast. Reusing the exact same provider (and therefore the same
/// underlying `reqwest::Client` connection pool) between warm and fire is
/// the whole point: building a fresh `ProviderBuilder::on_http()` per
/// broadcast, which is what this file did before, throws away the warmed
/// TCP/TLS connection and pays for a new handshake at exactly the moment
/// that handshake is most expensive.
pub type HttpProvider = RootProvider;

/// One wallet's transaction, signed and serialized ahead of the trigger.
/// `fire_prepared` does nothing with this but write `raw_tx` to a socket —
/// no signing, no nonce/gas lookup, no RPC read happens in that path.
pub struct PreparedWallet {
    pub address: Address,
    pub raw_tx: Vec<u8>,
    pub tx_hash: TxHash,
    /// STEP 13e — when this wallet's tx was actually signed. `Instant`,
    /// not a wall-clock timestamp: only ever compared against another
    /// `Instant` from the same process to compute a duration, never
    /// serialized or shown as an absolute time, so monotonic-clock
    /// semantics (immune to a clock adjustment mid-fire) are exactly
    /// what's wanted here. Chain-agnostic by construction — nothing
    /// below reads chain id or network name, only wall-clock deltas.
    pub prepared_at: std::time::Instant,
}

/// Output of the prepare phase: every wallet's signed tx, plus the warmed
/// providers they'll be broadcast over. Kept together because both exist
/// for the same reason (eliminating fire-time latency) and both go stale
/// together — a re-arm always rebuilds both, never reuses one without the
/// other (a stale nonce/gas-price signature replayed against a freshly
/// warmed connection is still a stale, possibly-reverting transaction).
pub struct PreparedFire {
    pub wallets: Vec<PreparedWallet>,
    pub providers: Vec<HttpProvider>,
}

/// Opens a connection to every configured RPC endpoint and issues a cheap
/// read call (`eth_blockNumber`) to force the TCP/TLS handshake to
/// complete now rather than at fire time. Mirrors `connection-warmer.ts`
/// from the reference implementation audited in step 1: the handshake is
/// the point, not the response, so a failed warm call is logged and
/// otherwise ignored — the provider is still returned and usable, it'll
/// just pay for its own handshake at fire time instead of now.
pub async fn warm_connections(cfg: &Config, bus: &EventBus) -> Vec<HttpProvider> {
    let mut providers = Vec::with_capacity(cfg.http_rpc_urls.len());

    for url in &cfg.http_rpc_urls {
        let parsed = match url.parse() {
            Ok(u) => u,
            Err(e) => {
                bus::log(bus, "error", format!("skipping unparseable RPC url {url}: {e}"));
                continue;
            }
        };
        let provider = ProviderBuilder::new().disable_recommended_fillers().connect_http(parsed);

        match provider.get_block_number().await {
            Ok(_) => bus::log(bus, "info", format!("connection warmed: {url}")),
            Err(e) => bus::log(
                bus,
                "warn",
                format!("connection warm failed for {url} (will still be used, just cold): {e}"),
            ),
        }
        providers.push(provider);
    }

    providers
}

/// Builds and signs one raw transaction per wallet ahead of the trigger:
/// chain id, gas price, and a representative gas estimate are each fetched
/// once here (not per-wallet, not per-broadcast), nonce comes from each
/// wallet's locally-tracked counter (see wallet.rs — no per-wallet RPC
/// round trip either), and every wallet's tx is signed via its own local
/// signer. The result is cached raw bytes; `fire_prepared` does no
/// signing and no RPC read.
///
/// Does NOT advance `next_nonce` — takes `&[ManagedWallet]`, not `&mut`,
/// on purpose. This can be called repeatedly before anything it produces
/// is ever broadcast (poll_state's periodic re-prepare loop calls it every
/// `POLL_STATE_REPREPARE_INTERVAL_SECS` while armed), and only the most
/// recent call's output is ever fired. The caller advances `next_nonce`
/// exactly once, at the moment it actually commits to broadcasting a
/// prepared batch — see `control_loop`'s `ControlMsg::FireNow` handler.
pub async fn prepare_fire(
    cfg: &Config,
    wallets: &[ManagedWallet],
    contract: Address,
    calldata: &[u8],
    mint_value_per_wallet: U256,
    providers: &[HttpProvider],
    bus: &EventBus,
) -> Result<Vec<PreparedWallet>> {
    let reader = providers
        .first()
        .context("prepare_fire called with no warmed RPC providers")?;

    let chain_id = reader.get_chain_id().await.context("fetching chain id")?;

    // Same "current gas price as a priority-fee baseline" approach this
    // file used before pre-signing existed; the only change is doing the
    // fetch once here instead of once per fire.
    let base_fee = reader.get_gas_price().await.context("fetching gas price")?; // legacy fallback if EIP-1559 fee history call fails
    let priority_fee_wei = ((base_fee as f64) * cfg.priority_fee_multiplier) as u128;
    let cap_wei = (cfg.max_priority_fee_gwei_cap * 1e9) as u128;
    let priority_fee_wei = priority_fee_wei.min(cap_wei);

    // Calldata and value are identical for every wallet (see the gas-jitter
    // comment below for why gas price, not calldata, is the anti-clustering
    // lever), so one eth_estimateGas covers all of them.
    // gas_limit_headroom_pct has existed in config since before this file
    // compiled but was never read — gas_limit was never set on the tx at
    // all, which only worked previously because `send_transaction`'s filler
    // stack silently estimated it at send time. Signing ourselves means
    // there's no filler left to do that, so this is now load-bearing, not
    // decorative.
    let mut probe_tx = TransactionRequest::default()
        .to(contract)
        .input(calldata.to_vec().into())
        .value(mint_value_per_wallet);
    if let Some(from) = wallets.first().map(|w| w.address) {
        probe_tx = probe_tx.from(from);
    }
    let estimated_gas = reader
        .estimate_gas(probe_tx)
        .await
        .context("estimating gas")?;
    let gas_limit = estimated_gas + (estimated_gas * cfg.gas_limit_headroom_pct) / 100;

    let mut prepared = Vec::with_capacity(wallets.len());

    for w in wallets.iter() {
        // Read only — do NOT advance next_nonce here. This function can be
        // (and, in poll_state mode, routinely is) called multiple times
        // before any of its output ever gets broadcast, via the periodic
        // re-prepare loop in control_loop. Advancing on every prepare would
        // burn a nonce slot per cycle that nothing ever fills on-chain —
        // confirmed live against Sepolia during the step 5 dry run: two
        // re-prepare cycles left this wallet's cached tx signed for nonce 1
        // while its actual on-chain nonce was still 0, which would have
        // left the eventual real broadcast stuck pending forever (Ethereum
        // requires strictly sequential nonces per sender). The advance now
        // happens exactly once, in control_loop, at the moment a prepared
        // batch is actually committed to firing — see FireNow's handler.
        let nonce = w.next_nonce;

        // Jitter is intentional: identical gas price + identical block from
        // N wallets is a trivial sybil-clustering signature. A few % of gas
        // spread costs almost nothing on a free mint and buys meaningfully
        // better odds of surviving allowlist-eligibility heuristics on
        // future drops from the same project.
        let gas_jitter_pct: i64 = rand::thread_rng()
            .gen_range(-(cfg.gas_jitter_pct as i64)..=(cfg.gas_jitter_pct as i64));
        let wallet_priority_fee = apply_pct_jitter(priority_fee_wei, gas_jitter_pct);

        // A ceiling with headroom, not a live read echoed straight back:
        // base_fee can move between now and whenever the trigger actually
        // fires — that gap is the entire premise of pre-signing — so the
        // cap has to tolerate a couple of blocks of increase rather than
        // being pinned to what get_gas_price() returned this instant.
        // base*2 + priority is the same ceiling heuristic the reference
        // implementation's wizard suggests (wizard.ts, audited in step 1).
        let max_fee_per_gas = wallet_priority_fee + base_fee.saturating_mul(2);

        let mut tx = TransactionRequest::default()
            .to(contract)
            .input(calldata.to_vec().into())
            .nonce(nonce)
            .gas_limit(gas_limit)
            .max_fee_per_gas(max_fee_per_gas)
            .max_priority_fee_per_gas(wallet_priority_fee)
            .value(mint_value_per_wallet);
        // No inherent with_chain_id() setter exists in this alloy version
        // (only chain_id() as a getter) — the field itself is public, and
        // setting it directly is preferable to leaving it unset: an unset
        // chain_id silently defaults to 1 (mainnet) inside build_1559(),
        // which would be wrong and non-obvious on every other chain.
        tx.chain_id = Some(chain_id);

        let eth_wallet: EthereumWallet = wallet_to_eth_wallet(w);
        let envelope = tx
            .build(&eth_wallet)
            .await
            .with_context(|| format!("signing tx for wallet {:#x}", w.address))?;

        let raw_tx = envelope.encoded_2718();
        let tx_hash = *envelope.tx_hash();
        let prepared_at = std::time::Instant::now();

        // STEP 15 FOLLOW-UP — actual gas price used was previously
        // computed but never logged anywhere inspectable after the fact
        // (not tracing, not audit.log), which meant a live investigation
        // into whether slow dispatch_to_inclusion_ms was gas-price-
        // related had no data to check it against. Now on this line,
        // reaching journalctl (tracing::info! is durable there, unlike
        // bus::log — see step 17's finding on that same gap).
        info!(
            address = %w.address, %tx_hash, nonce,
            base_fee_wei = base_fee,
            wallet_priority_fee_wei = wallet_priority_fee,
            max_fee_per_gas_wei = max_fee_per_gas,
            "wallet prepared (signed, not broadcast)"
        );
        bus::log(
            bus,
            "info",
            format!("prepared {:#x} — nonce {nonce}, tx {tx_hash:#x}", w.address),
        );

        prepared.push(PreparedWallet {
            address: w.address,
            raw_tx,
            prepared_at,
            tx_hash,
        });
    }

    Ok(prepared)
}

/// Broadcasts already-signed transactions to every warmed RPC endpoint
/// concurrently (first confirmation wins, rest are wasted but harmless —
/// same signed tx, same hash). No signing, no nonce/gas lookups, no
/// provider construction here — see `prepare_fire` and `warm_connections`.
/// The only thing decided at this point is the per-wallet broadcast-timing
/// jitter, which is about *when* the already-built bytes hit the wire, not
/// what's in them, so there's no reason to fix it before the trigger.
///
/// STEP 13e — `trigger_detected_at` is when the CALLER first knew firing
/// should happen (a watcher tripping, or a manual/copymint fire request
/// being received) — see `main.rs`'s `control_loop` for where each of
/// those is actually captured. Everything here is plain wall-clock
/// `Instant` math with zero chain-specific logic, so the resulting
/// `trigger_to_dispatch_ms`/`send_to_ack_ms`/`dispatch_to_inclusion_ms`
/// on `ServerEvent::MintResult` are directly comparable across mainnet,
/// Sepolia, and Robinhood Chain runs — that's what makes step 13d's dry
/// run produce a real number to check against MintDash's own published
/// Robinhood Chain figures (send→ack p50 117ms, mintDuration p50 136ms —
/// see CLAUDE.md's step 13 section), not a mainnet number with nothing
/// to compare it to.
/// STEP 14a — one per-provider broadcast attempt's outcome, once it's
/// gotten far enough to know send_to_ack_ms (i.e. `send_raw_transaction`
/// itself succeeded). Kept distinct from a plain `Err` (which means the
/// broadcast itself was rejected/never went out) so `fire_prepared` can
/// tell "definitely never happened" apart from "happened, but we
/// couldn't confirm it in time" — two states with very different
/// operational meaning that must not collapse into the same generic
/// failure bucket.
enum SendAttemptOutcome {
    // Boxed for the same reason as inclusion::InclusionOutcome's own
    // Included variant — clippy's large_enum_variant under CI's real
    // `-D warnings`, confirmed live, not just plain `cargo clippy`.
    Included {
        receipt: Box<TransactionReceipt>,
        send_to_ack_ms: u64,
        dispatch_to_inclusion_ms: u64,
        method: &'static str,
    },
    TimedOut {
        send_to_ack_ms: u64,
        method: &'static str,
    },
}

pub async fn fire_prepared(
    cfg: &Config,
    prepared: &[PreparedWallet],
    providers: &[HttpProvider],
    bus: &EventBus,
    trigger_detected_at: std::time::Instant,
    block_ticker: Option<crate::inclusion::BlockTicker>,
) -> Result<()> {
    let _ = bus.send(ServerEvent::TriggerFired { manual: false });

    if providers.is_empty() {
        anyhow::bail!("no warmed RPC providers to broadcast to");
    }

    // STEP 14a — sized from config, not hardcoded; see Config::block_time_ms/
    // inclusion_timeout_ms's own doc comments. `block_ticker` (established
    // once at Arm time by main.rs's control_loop — see its own doc comment
    // for why NOT here, at fire time) selects PUSH when `Some`, HTTP-poll
    // fallback at `poll_interval` when `None`.
    let poll_interval = std::time::Duration::from_millis(cfg.block_time_ms);
    let inclusion_timeout = std::time::Duration::from_millis(cfg.inclusion_timeout_ms);
    // Captured as an owned value (not `cfg` itself) so it can move into
    // the 'static-bound `tokio::spawn`ed tasks below — `cfg: &Config`
    // only lives for this function call, `cfg.inclusion_timeout_ms: u64`
    // is Copy and has no such lifetime constraint.
    let inclusion_timeout_ms = cfg.inclusion_timeout_ms;

    let mut handles = Vec::with_capacity(prepared.len());

    for pw in prepared {
        let raw_tx = pw.raw_tx.clone();
        let address = pw.address;
        let expected_hash = pw.tx_hash;
        let prepared_at = pw.prepared_at;
        let providers = providers.to_vec(); // RootProvider is a cheap Arc-clone — same pooled connections
        let bus = bus.clone();
        let block_ticker = block_ticker.clone();

        let jitter_ms: u64 = rand::thread_rng().gen_range(cfg.jitter_ms_min..=cfg.jitter_ms_max);

        handles.push(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;

            // Measured AFTER the jitter sleep, deliberately — jitter is an
            // intentional anti-clustering delay this codebase chose to
            // pay, not fire-path overhead to report as latency. This is
            // the moment these bytes actually start going out the door.
            let dispatch_started = std::time::Instant::now();
            let trigger_to_dispatch_ms = dispatch_started.saturating_duration_since(trigger_detected_at).as_millis() as u64;
            let prepare_age_ms = dispatch_started.saturating_duration_since(prepared_at).as_millis() as u64;

            // Race the same raw bytes across every warmed RPC; first to
            // land wins. Unlike the old select_ok-based version, every
            // provider's result is collected (not just whichever error
            // `select_ok` happened to return last) — a rejection reason
            // like "nonce too low" or "max fee per gas less than block
            // base fee" is exactly what someone watching the control deck
            // needs to see, per wallet, not an arbitrary single provider's
            // opinion when providers disagree on why it failed.
            //
            // get_receipt() (not watch()) deliberately: watch() only
            // confirms inclusion in a block, not execution status, so a
            // transaction that lands but reverts (insufficient payment,
            // maxTotalMintable already hit, drop ended between prepare and
            // fire, ...) was being reported as success — the single most
            // important number this tool reports being wrong. This is one
            // more RPC call than watch() makes, but it happens here, after
            // send_raw_transaction has already dispatched the bytes and
            // we're already waiting on confirmation either way — nothing
            // is added to the actual broadcast/dispatch step itself.
            let sends = providers.iter().enumerate().map(|(i, provider)| {
                let raw_tx = raw_tx.clone();
                let block_ticker = block_ticker.clone();
                async move {
                    // Inner block explicitly typed as anyhow::Result so `?`
                    // has one concrete error type to convert
                    // send_raw_transaction's errors into; the index is
                    // attached after, on the way out, since that error
                    // type otherwise doesn't unify with
                    // (usize, anyhow::Error) directly.
                    //
                    // STEP 13e: send_to_ack_ms is captured the instant
                    // send_raw_transaction itself returns — the RPC
                    // accepting the raw bytes and handing back a pending-tx
                    // handle, NOT inclusion — this is the number directly
                    // comparable to MintDash's published "send→ack" figure.
                    //
                    // STEP 14a: inclusion detection no longer uses
                    // `PendingTransactionBuilder::get_receipt()` (whose
                    // internal poll interval, not real inclusion speed, is
                    // what step 13f found was dominating
                    // dispatch_to_inclusion_ms) — `inclusion::wait_for_receipt`
                    // instead, PUSH (block-arrival driven) when
                    // `block_ticker` is `Some`, HTTP-poll fallback
                    // otherwise. See inclusion.rs's own doc comment for
                    // the full design and why it's a separate module from
                    // watcher.rs's per-block mint-state check (step 13c).
                    let result: Result<SendAttemptOutcome, anyhow::Error> = async {
                        let _pending = provider.send_raw_transaction(&raw_tx).await?;
                        let send_to_ack_ms = dispatch_started.elapsed().as_millis() as u64;
                        match crate::inclusion::wait_for_receipt(
                            provider,
                            expected_hash,
                            block_ticker,
                            poll_interval,
                            inclusion_timeout,
                        )
                        .await
                        {
                            crate::inclusion::InclusionOutcome::Included { receipt, method } => {
                                let dispatch_to_inclusion_ms = dispatch_started.elapsed().as_millis() as u64;
                                Ok(SendAttemptOutcome::Included { receipt, send_to_ack_ms, dispatch_to_inclusion_ms, method })
                            }
                            crate::inclusion::InclusionOutcome::TimedOut { method } => {
                                Ok(SendAttemptOutcome::TimedOut { send_to_ack_ms, method })
                            }
                        }
                    }
                    .await;
                    result.map_err(|e| (i, e))
                }
            });

            let results = join_all(sends).await;

            // Priority: any provider that actually got an Included receipt
            // wins outright, regardless of whether other providers timed
            // out or failed to broadcast at all.
            let included = results.iter().find_map(|r| match r.as_ref().ok() {
                Some(SendAttemptOutcome::Included { receipt, send_to_ack_ms, dispatch_to_inclusion_ms, method }) => {
                    Some((receipt, *send_to_ack_ms, *dispatch_to_inclusion_ms, *method))
                }
                _ => None,
            });

            if let Some((receipt, send_to_ack_ms, dispatch_to_inclusion_ms, method)) = included {
                let tx_hash = receipt.transaction_hash;
                // The RPC's returned hash should always equal the hash
                // computed at prepare time — raw_tx fully determines it,
                // nothing about broadcast can change it. A mismatch would
                // mean raw_tx got corrupted (clone/serialize bug) between
                // prepare and fire, so it's worth surfacing loudly rather
                // than trusting whichever hash happened to come back.
                if tx_hash != expected_hash {
                    error!(%address, %tx_hash, %expected_hash, "broadcast tx hash does not match the one computed at prepare time");
                }

                if receipt.status() {
                    info!(%address, %tx_hash, send_to_ack_ms, dispatch_to_inclusion_ms, method, "mint confirmed");
                    let _ = bus.send(ServerEvent::MintResult {
                        address: format!("{address:#x}"),
                        success: true,
                        detail: format!("{tx_hash:#x}"),
                        trigger_to_dispatch_ms: Some(trigger_to_dispatch_ms),
                        prepare_age_ms,
                        send_to_ack_ms: Some(send_to_ack_ms),
                        dispatch_to_inclusion_ms: Some(dispatch_to_inclusion_ms),
                    });
                } else {
                    // Included, but reverted. Not a broadcast failure —
                    // every RPC accepted it — so it doesn't belong in the
                    // "no reason attached" aggregation below; it's a
                    // distinct, confirmed-on-chain outcome. No revert-
                    // reason decoding here (would need an extra eth_call
                    // trace replay); block number + gas used is enough to
                    // confirm "this really happened" without a whole
                    // subsystem for why. Timing is still real and still
                    // reported — a revert still measures real send→ack and
                    // dispatch→inclusion numbers, same as a success.
                    let detail = format!(
                        "reverted on-chain — tx {tx_hash:#x}, block {}, gas used {}",
                        receipt.block_number.map_or("?".to_string(), |b| b.to_string()),
                        receipt.gas_used,
                    );
                    error!(%address, %tx_hash, method, "mint tx included but reverted");
                    let _ = bus.send(ServerEvent::MintResult {
                        address: format!("{address:#x}"),
                        success: false,
                        detail,
                        trigger_to_dispatch_ms: Some(trigger_to_dispatch_ms),
                        prepare_age_ms,
                        send_to_ack_ms: Some(send_to_ack_ms),
                        dispatch_to_inclusion_ms: Some(dispatch_to_inclusion_ms),
                    });
                }
            } else if let Some((send_to_ack_ms, method)) = results.iter().find_map(|r| match r.as_ref().ok() {
                Some(SendAttemptOutcome::TimedOut { send_to_ack_ms, method }) => Some((*send_to_ack_ms, *method)),
                _ => None,
            }) {
                // STEP 14a — distinct from BOTH success and revert: at
                // least one provider ack'd the broadcast (this tx really
                // did go out), but no receipt showed up within
                // Config::inclusion_timeout_ms via either detection path.
                // This does NOT mean the tx failed — it may still confirm
                // later — so it must never be conflated with "all RPC
                // broadcasts failed" below, which means the tx never left
                // this process at all.
                let detail = format!(
                    "inclusion not confirmed within {inclusion_timeout_ms}ms (detection method={method}) — \
                     tx {expected_hash:#x} may still be pending; check a block explorer before assuming it failed"
                );
                tracing::warn!(%address, tx_hash = %expected_hash, method, timeout_ms = inclusion_timeout_ms, "inclusion detection timed out");
                let _ = bus.send(ServerEvent::MintResult {
                    address: format!("{address:#x}"),
                    success: false,
                    detail,
                    trigger_to_dispatch_ms: Some(trigger_to_dispatch_ms),
                    prepare_age_ms,
                    send_to_ack_ms: Some(send_to_ack_ms),
                    dispatch_to_inclusion_ms: None, // never resolved — that's the whole point of this branch
                });
            } else {
                let detail = results
                    .iter()
                    .map(|r| match r {
                        Ok(_) => unreachable!("handled by the branches above"),
                        Err((i, e)) => format!("rpc[{i}]: {e}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                error!(%address, error = %detail, "all RPC broadcasts failed for this wallet");
                // No ack/inclusion time exists when every RPC rejected the
                // broadcast outright — but trigger_to_dispatch_ms is still
                // real and worth keeping, since it's independent of what
                // happened after dispatch.
                let _ = bus.send(ServerEvent::MintResult {
                    address: format!("{address:#x}"),
                    success: false,
                    detail,
                    trigger_to_dispatch_ms: Some(trigger_to_dispatch_ms),
                    prepare_age_ms,
                    send_to_ack_ms: None,
                    dispatch_to_inclusion_ms: None,
                });
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

#[cfg(test)]
mod tests {
    use super::apply_pct_jitter;

    #[test]
    fn zero_jitter_is_a_noop() {
        assert_eq!(apply_pct_jitter(1_000_000, 0), 1_000_000);
        assert_eq!(apply_pct_jitter(0, 0), 0);
    }

    #[test]
    fn positive_jitter_increases_by_percent() {
        assert_eq!(apply_pct_jitter(1_000_000, 8), 1_080_000);
        // +100% doubles it — the boundary where delta equals the base value.
        assert_eq!(apply_pct_jitter(1_000_000, 100), 2_000_000);
    }

    #[test]
    fn negative_jitter_decreases_by_percent() {
        assert_eq!(apply_pct_jitter(1_000_000, -8), 920_000);
        // -100% is the boundary where delta exactly cancels the base value.
        assert_eq!(apply_pct_jitter(1_000_000, -100), 0);
    }

    #[test]
    fn negative_jitter_never_produces_a_negative_gas_value() {
        // Anything more negative than -100% must saturate at 0, not
        // underflow/wrap. This function returns u128 specifically to feed
        // maxPriorityFeePerGas — a wrapped-around near-u128::MAX value from
        // an unsigned underflow here is exactly the kind of bug that burns
        // real money on a live mint, not a theoretical concern.
        assert_eq!(apply_pct_jitter(1_000_000, -150), 0);
        assert_eq!(apply_pct_jitter(1_000_000, -1_000), 0);
        assert_eq!(apply_pct_jitter(0, -100), 0);
        assert_eq!(apply_pct_jitter(0, -1_000), 0);
    }
}
