use crate::bus::{self, EventBus, ServerEvent};
use crate::config::{self, Config};
use crate::wallet::{wallet_to_eth_wallet, ManagedWallet};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::{EthereumWallet, NetworkTransactionBuilder};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use anyhow::{Context, Result};
use futures::future::join_all;
use rand::Rng;
use std::sync::Arc;
use tracing::{error, info, warn};

pub type HttpProvider = RootProvider;

pub struct PreparedWallet {
    pub address: Address,
    /// Cheap to clone across sequencer + backup fan-out; send_raw_transaction takes &[u8].
    pub raw_tx: Arc<[u8]>,
    pub tx_hash: TxHash,
    pub prepared_at: std::time::Instant,
}

pub struct PreparedFire {
    pub wallets: Vec<PreparedWallet>,
    pub providers: Vec<HttpProvider>,
    /// Warmed sequencer HTTP provider if Config::sequencer_http_url was set at arm time.
    pub sequencer: Option<HttpProvider>,
}

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

/// Same arm-time TCP/TLS handshake as `warm_connections`, for the optional sequencer URL.
/// Returns `None` when unset or unparseable.
///
/// P0 follow-up 21b — the naive health check here used to be
/// `provider.get_block_number()` (`eth_blockNumber`), which was wrong for
/// this specific endpoint and logged a false "warm failed" warning on
/// every single Arm even when the connection was completely fine.
/// Confirmed directly, not assumed: Robinhood Chain's real sequencer
/// submit endpoint (`https://sequencer.{mainnet,testnet}.chain.robinhood.com`)
/// only implements `eth_sendRawTransaction` (and
/// `eth_sendRawTransactionConditional`) — every general read method tried
/// (`eth_chainId`, `eth_blockNumber`, `eth_gasPrice`, `net_version`,
/// `web3_clientVersion`, `eth_syncing`, `eth_getBalance`, `eth_call`,
/// `eth_estimateGas`, `eth_maxPriorityFeePerGas`,
/// `eth_getTransactionCount`, `eth_getTransactionReceipt`) returns a
/// `-32601 method does not exist` JSON-RPC error. There is no cheap read
/// call this endpoint will ever answer with a real `Ok`, so a
/// success/failure health check modeled on `warm_connections`'s
/// `eth_blockNumber` probe can't work here — the fix is not a different
/// method, there isn't one.
///
/// Instead: send a harmless probe method anyway (still forces the real
/// TCP/TLS handshake — the endpoint has to accept the connection and
/// parse a well-formed JSON-RPC request to send back ANY response, error
/// or not), but classify the result by what KIND of failure comes back.
/// A well-formed JSON-RPC error response (`RpcError::ErrorResp` —
/// `is_error_resp()`) proves the connection and handshake succeeded; the
/// -32601 is simply this write-only endpoint correctly saying "not
/// implemented," not a warm failure. Only a genuine transport-level
/// error (DNS failure, connection refused, TLS failure, timeout — none
/// of which produce a JSON-RPC error payload at all) is a real warm
/// failure worth logging as a warning.
pub async fn warm_sequencer(cfg: &Config, bus: &EventBus) -> Option<HttpProvider> {
    if cfg.sequencer_http_url.is_empty() {
        return None;
    }
    let url = &cfg.sequencer_http_url;
    let parsed = match url.parse() {
        Ok(u) => u,
        Err(e) => {
            bus::log(bus, "error", format!("skipping unparseable sequencer url {url}: {e}"));
            return None;
        }
    };
    let provider = ProviderBuilder::new().disable_recommended_fillers().connect_http(parsed);
    match provider.raw_request::<_, serde_json::Value>("eth_chainId".into(), [(); 0]).await {
        Ok(_) => bus::log(bus, "info", format!("sequencer connection warmed: {url}")),
        Err(e) if e.is_error_resp() => bus::log(
            bus,
            "info",
            format!("sequencer connection warmed: {url} (write-only endpoint, as expected: {e})"),
        ),
        Err(e) => bus::log(
            bus,
            "warn",
            format!("sequencer warm failed for {url} (will still be used, just cold): {e}"),
        ),
    }
    Some(provider)
}

/// `None` means skip gen_range and skip sleep entirely (including sleep(0)).
pub(crate) fn sample_fire_jitter_ms(jitter_ms_min: u64, jitter_ms_max: u64) -> Option<u64> {
    if jitter_ms_max == 0 {
        None
    } else {
        Some(rand::thread_rng().gen_range(jitter_ms_min..=jitter_ms_max))
    }
}

/// STEP 34 — `prepare_fire`'s three read calls (`get_chain_id`,
/// `get_gas_price`, `estimate_gas`) had no timeout of their own, same gap
/// class as STEP 33's `send_raw_transaction` — but critically EARLIER in
/// the real call chain: `prepare_fire` runs synchronously inside
/// `control_loop`'s single `while let Some(msg) = control_rx.recv().await`
/// loop (called directly from the `ControlMsg::Prepare` handler, from
/// `FireNow`'s no-prior-prepare fallback, and from `FireCopymint`) — there
/// is no `tokio::spawn` around it. A stall in any of these three calls
/// therefore doesn't just hang one task: it hangs the ENTIRE control loop,
/// which is the only consumer of `state.control_tx` — no further Arm,
/// Disarm, Prepare, or FireNow message is ever processed again. And
/// because this function's first log line (`"wallet prepared"`) only
/// fires AFTER all three reads succeed, a stall here produces genuinely
/// zero log output — indistinguishable, from the outside, from the
/// process itself having died. This is a stronger match for a reported
/// "TRIGGER logged, then total silence" live regression than STEP 33's
/// fix alone (which only covers `fire_prepared`, downstream of this
/// function): STEP 33's own new timeout/logging would have produced
/// SOME output if that code had ever been reached, and it didn't.
const PREPARE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Wraps one read-only provider call (`get_chain_id`/`get_gas_price`/
/// `estimate_gas` — all `Result<T, E>`-returning `Provider` methods) in a
/// timeout, turning a silent, indefinite stall into a loud, `anyhow`-
/// context-carrying error the existing `?`-based callers in
/// `prepare_fire` already know how to propagate and log. Takes `timeout`
/// as a parameter (rather than hardcoding `PREPARE_READ_TIMEOUT`
/// internally) so it's independently testable with a short duration —
/// same reasoning as STEP 33's `send_raw_tx_with_timeout`.
async fn timed_read<T, E, F>(fut: F, what: &str, timeout: std::time::Duration) -> Result<T>
where
    // alloy's provider call builders (e.g. `estimate_gas`'s `EthCall`)
    // implement `IntoFuture`, not `Future` directly — `.await`ing them
    // works via that conversion, so this helper accepts the same.
    F: std::future::IntoFuture<Output = std::result::Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(timeout, fut.into_future()).await {
        Ok(inner) => inner.with_context(|| what.to_string()),
        Err(_elapsed) => Err(anyhow::anyhow!("{what} timed out after {}ms", timeout.as_millis())),
    }
}

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

    let chain_id = timed_read(reader.get_chain_id(), "fetching chain id", PREPARE_READ_TIMEOUT).await?;

    let base_fee = timed_read(reader.get_gas_price(), "fetching gas price", PREPARE_READ_TIMEOUT).await?;
    let priority_fee_wei = ((base_fee as f64) * cfg.priority_fee_multiplier) as u128;
    let cap_wei = (cfg.max_priority_fee_gwei_cap * 1e9) as u128;
    let priority_fee_wei = priority_fee_wei.min(cap_wei);

    let mut probe_tx = TransactionRequest::default()
        .to(contract)
        .input(calldata.to_vec().into())
        .value(mint_value_per_wallet);
    if let Some(from) = wallets.first().map(|w| w.address) {
        probe_tx = probe_tx.from(from);
    }
    let estimated_gas = timed_read(reader.estimate_gas(probe_tx), "estimating gas", PREPARE_READ_TIMEOUT).await?;
    let gas_limit = estimated_gas + (estimated_gas * cfg.gas_limit_headroom_pct) / 100;

    let mut prepared = Vec::with_capacity(wallets.len());

    for w in wallets.iter() {
        let nonce = w.next_nonce;

        let gas_jitter_pct: i64 = if cfg.gas_jitter_pct == 0 {
            0
        } else {
            rand::thread_rng().gen_range(-(cfg.gas_jitter_pct as i64)..=(cfg.gas_jitter_pct as i64))
        };
        let wallet_priority_fee = apply_pct_jitter(priority_fee_wei, gas_jitter_pct);
        let max_fee_per_gas = wallet_priority_fee + base_fee.saturating_mul(2);

        let mut tx = TransactionRequest::default()
            .to(contract)
            .input(calldata.to_vec().into())
            .nonce(nonce)
            .gas_limit(gas_limit)
            .max_fee_per_gas(max_fee_per_gas)
            .max_priority_fee_per_gas(wallet_priority_fee)
            .value(mint_value_per_wallet);
        tx.chain_id = Some(chain_id);

        let eth_wallet: EthereumWallet = wallet_to_eth_wallet(w);
        let envelope = tx
            .build(&eth_wallet)
            .await
            .with_context(|| format!("signing tx for wallet {:#x}", w.address))?;

        let raw_tx: Arc<[u8]> = envelope.encoded_2718().into();
        let tx_hash = *envelope.tx_hash();
        let prepared_at = std::time::Instant::now();

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

/// STEP 33 — `send_raw_transaction(...).await` has no timeout of its own
/// anywhere in this codebase; alloy's underlying reqwest client is built
/// with no `.timeout()` set (`ProviderBuilder::new().connect_http`, see
/// `warm_connections`/`warm_sequencer`). A stalled TCP connection (no
/// response, no RST — a real failure mode, not a hypothetical) would
/// previously hang this `.await` forever, with nothing bounding it and
/// nothing ever logged, since the log call for that attempt happens only
/// AFTER the `.await` resolves. 10s is far above every latency this
/// project has ever measured for a send (n=100 p99 was 414ms — see
/// CLAUDE.md's step 28 (final close, corrected)) and far below "hang
/// forever" — generous enough to never fire on a slow-but-real network
/// path, tight enough to turn a genuine stall into a loud, logged
/// timeout instead of silence.
const SEND_RAW_TX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// STEP 33 — outcome of one timeout-wrapped `send_raw_transaction` call.
/// Shared by both the sequencer and backup send paths (they were near-
/// identical inline `match`es before this — real duplication, not a
/// premature abstraction) and independently unit-testable with a short
/// `timeout` in tests, without needing to wait out the real
/// `SEND_RAW_TX_TIMEOUT` (10s) to prove the timeout branch works.
enum SendAttempt {
    Acked,
    RpcError(anyhow::Error),
    TimedOut,
}

/// Never hangs longer than `timeout`, regardless of how the underlying
/// connection behaves — this is the actual fix for STEP 33's silent hang:
/// previously nothing bounded `send_raw_transaction(...).await` at all.
async fn send_raw_tx_with_timeout(provider: &HttpProvider, raw_tx: &[u8], timeout: std::time::Duration) -> SendAttempt {
    match tokio::time::timeout(timeout, provider.send_raw_transaction(raw_tx)).await {
        Ok(Ok(_)) => SendAttempt::Acked,
        Ok(Err(e)) => SendAttempt::RpcError(e.into()),
        Err(_elapsed) => SendAttempt::TimedOut,
    }
}

enum SendAttemptOutcome {
    Included {
        receipt: Box<TransactionReceipt>,
        send_to_ack_ms: u64,
        dispatch_to_inclusion_ms: u64,
        method: &'static str,
        acked_url: String,
    },
    TimedOut {
        send_to_ack_ms: u64,
        method: &'static str,
        acked_url: String,
    },
}

fn prefer_sequencer_ack(
    sequencer_ack: Option<(u64, String)>,
    backup: Option<(u64, String)>,
) -> Option<(u64, String)> {
    sequencer_ack.or(backup)
}

/// STEP 28 (final) — classifies which path actually acked a send, for
/// `ServerEvent::MintResult`'s `ack_source` field. Compares the RAW
/// (unredacted) acked URL against the RAW configured `sequencer_url` —
/// exact string equality, not a substring/host heuristic, since that's
/// the only way to be certain rather than guessed (a backup RPC could
/// coincidentally share a host with the sequencer on some future
/// config). Never logs or persists the raw URLs it compares — only this
/// function's own `&'static str` return value crosses into the bus
/// event; the caller separately redacts `acked_url` before sending it.
fn classify_ack_source(sequencer_url: &str, acked_url: &str) -> &'static str {
    if !sequencer_url.is_empty() && acked_url == sequencer_url {
        "sequencer"
    } else {
        "backup"
    }
}

pub async fn fire_prepared(
    cfg: &Config,
    prepared: &[PreparedWallet],
    providers: &[HttpProvider],
    sequencer: Option<&HttpProvider>,
    bus: &EventBus,
    trigger_detected_at: std::time::Instant,
    block_ticker: Option<crate::inclusion::BlockTicker>,
) -> Result<()> {
    let _ = bus.send(ServerEvent::TriggerFired { manual: false });

    if providers.is_empty() && sequencer.is_none() {
        anyhow::bail!("no warmed RPC providers to broadcast to");
    }

    let poll_interval = std::time::Duration::from_millis(cfg.block_time_ms);
    let inclusion_timeout = std::time::Duration::from_millis(cfg.inclusion_timeout_ms);
    let inclusion_timeout_ms = cfg.inclusion_timeout_ms;
    let jitter_ms_min = cfg.jitter_ms_min;
    let jitter_ms_max = cfg.jitter_ms_max;
    let sequencer_url = cfg.sequencer_http_url.clone();
    let backup_urls = cfg.http_rpc_urls.clone();

    let mut handles = Vec::with_capacity(prepared.len());

    for pw in prepared {
        let raw_tx = pw.raw_tx.clone();
        let address = pw.address;
        let expected_hash = pw.tx_hash;
        let prepared_at = pw.prepared_at;
        let providers = providers.to_vec();
        let sequencer = sequencer.cloned();
        let sequencer_url = sequencer_url.clone();
        let backup_urls = backup_urls.clone();
        let bus = bus.clone();
        let block_ticker = block_ticker.clone();

        let handle_address = address;
        handles.push((handle_address, tokio::spawn(async move {
            // P0.1 — jitter_ms_max == 0 skips gen_range and skips sleep entirely. Do not sleep(0).
            if let Some(jitter_ms) = sample_fire_jitter_ms(jitter_ms_min, jitter_ms_max) {
                if jitter_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;
                }
            }

            let dispatch_started = std::time::Instant::now();
            let trigger_to_dispatch_ms = dispatch_started.saturating_duration_since(trigger_detected_at).as_millis() as u64;
            let prepare_age_ms = dispatch_started.saturating_duration_since(prepared_at).as_millis() as u64;

            // STEP 28 (final) — every broadcast target's own attempt is
            // logged here, immediately, regardless of whether it ends up
            // being the wallet's "winning" ack — not just whichever URL
            // `prefer_sequencer_ack` later picks. This is the actual
            // missing instrumentation a prior benchmark's journalctl
            // review couldn't answer from: without a real per-attempt
            // record, a sequencer that was silently absent (never
            // configured/warmed for this fire) looks IDENTICAL in the
            // logs to one that raced fairly and lost on latency — two
            // different bugs needing two different fixes, and no way to
            // tell them apart after the fact without this. Pure logging
            // added to results already being computed on the hot path —
            // no new RPC calls, no new awaits, nothing that could slow
            // fire_prepared down.
            if sequencer.is_none() {
                info!(
                    %address,
                    "no sequencer for this fire — sequencer_http_url is empty or unparseable \
                     (warm_sequencer never produced a provider at Arm time); broadcasting to \
                     backup RPCs only"
                );
            }

            let sequencer_fut = async {
                if let Some(seq) = sequencer.as_ref() {
                    let attempt_started = std::time::Instant::now();
                    // STEP 33 — timeout-wrapped: send_raw_transaction has no
                    // timeout of its own (see SEND_RAW_TX_TIMEOUT's doc
                    // comment) — a stalled connection here used to hang
                    // this future, and the whole per-wallet fire with it,
                    // forever with nothing ever logged.
                    match send_raw_tx_with_timeout(seq, raw_tx.as_ref(), SEND_RAW_TX_TIMEOUT).await {
                        SendAttempt::Acked => {
                            let ms = dispatch_started.elapsed().as_millis() as u64;
                            info!(
                                url = %config::redact_rpc_url(&sequencer_url),
                                send_to_ack_ms = ms,
                                latency_ms = attempt_started.elapsed().as_millis() as u64,
                                ok = true,
                                "sequencer send attempt"
                            );
                            Some((ms, sequencer_url.clone()))
                        }
                        SendAttempt::RpcError(e) => {
                            warn!(
                                url = %config::redact_rpc_url(&sequencer_url),
                                latency_ms = attempt_started.elapsed().as_millis() as u64,
                                ok = false,
                                error = %e,
                                "sequencer send attempt; fanning out to backup RPCs"
                            );
                            None
                        }
                        SendAttempt::TimedOut => {
                            warn!(
                                url = %config::redact_rpc_url(&sequencer_url),
                                latency_ms = attempt_started.elapsed().as_millis() as u64,
                                ok = false,
                                timeout_ms = SEND_RAW_TX_TIMEOUT.as_millis() as u64,
                                "sequencer send attempt timed out; fanning out to backup RPCs"
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            };

            let backup_fut = async {
                let sends = providers.iter().enumerate().map(|(i, provider)| {
                    let raw_tx = raw_tx.clone();
                    let block_ticker = block_ticker.clone();
                    let url = backup_urls.get(i).cloned().unwrap_or_else(|| format!("rpc[{i}]"));
                    async move {
                        let result: Result<SendAttemptOutcome, anyhow::Error> = async {
                            let attempt_started = std::time::Instant::now();
                            // STEP 33 — same timeout-wrapping as the
                            // sequencer path above, for the same reason:
                            // an unbounded send_raw_transaction().await
                            // here used to be able to hang this wallet's
                            // entire fire forever, silently.
                            match send_raw_tx_with_timeout(provider, raw_tx.as_ref(), SEND_RAW_TX_TIMEOUT).await {
                                SendAttempt::Acked => {}
                                SendAttempt::RpcError(e) => {
                                    warn!(
                                        url = %config::redact_rpc_url(&url),
                                        latency_ms = attempt_started.elapsed().as_millis() as u64,
                                        ok = false,
                                        error = %e,
                                        "backup RPC send attempt"
                                    );
                                    return Err(e);
                                }
                                SendAttempt::TimedOut => {
                                    warn!(
                                        url = %config::redact_rpc_url(&url),
                                        latency_ms = attempt_started.elapsed().as_millis() as u64,
                                        ok = false,
                                        timeout_ms = SEND_RAW_TX_TIMEOUT.as_millis() as u64,
                                        "backup RPC send attempt timed out"
                                    );
                                    anyhow::bail!(
                                        "send_raw_transaction to {} timed out after {}ms",
                                        config::redact_rpc_url(&url),
                                        SEND_RAW_TX_TIMEOUT.as_millis()
                                    );
                                }
                            }
                            let send_to_ack_ms = dispatch_started.elapsed().as_millis() as u64;
                            info!(
                                url = %config::redact_rpc_url(&url),
                                send_to_ack_ms,
                                latency_ms = attempt_started.elapsed().as_millis() as u64,
                                ok = true,
                                "backup RPC send attempt"
                            );
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
                                    Ok(SendAttemptOutcome::Included {
                                        receipt,
                                        send_to_ack_ms,
                                        dispatch_to_inclusion_ms,
                                        method,
                                        acked_url: url,
                                    })
                                }
                                crate::inclusion::InclusionOutcome::TimedOut { method } => {
                                    Ok(SendAttemptOutcome::TimedOut { send_to_ack_ms, method, acked_url: url })
                                }
                            }
                        }
                        .await;
                        result.map_err(|e| (i, e))
                    }
                });
                join_all(sends).await
            };

            let (sequencer_ack, mut results) = tokio::join!(sequencer_fut, backup_fut);

            if results.iter().all(|r| r.is_err()) {
                // P0 follow-up 18b — prefer a warmed backup provider for
                // polling (matches every other path in this file), but a
                // sequencer-only race_mode config (few or no backup RPCs —
                // exactly the shape this feature encourages) can leave
                // `providers` empty, or every backup send can fail outright.
                // The sequencer's own already-warmed connection is an
                // equally valid `eth_getTransactionReceipt` client for a tx
                // it just accepted — reuse it rather than falling through to
                // the "sequencer acked but inclusion was not confirmed"
                // branch below and reporting success: false for a tx that
                // may have actually landed.
                if let (Some((seq_ms, seq_url)), Some(provider)) =
                    (sequencer_ack.as_ref(), providers.first().or(sequencer.as_ref()))
                {
                    match crate::inclusion::wait_for_receipt(
                        provider,
                        expected_hash,
                        block_ticker.clone(),
                        poll_interval,
                        inclusion_timeout,
                    )
                    .await
                    {
                        crate::inclusion::InclusionOutcome::Included { receipt, method } => {
                            let dispatch_to_inclusion_ms = dispatch_started.elapsed().as_millis() as u64;
                            results.push(Ok(SendAttemptOutcome::Included {
                                receipt,
                                send_to_ack_ms: *seq_ms,
                                dispatch_to_inclusion_ms,
                                method,
                                acked_url: seq_url.clone(),
                            }));
                        }
                        crate::inclusion::InclusionOutcome::TimedOut { method } => {
                            results.push(Ok(SendAttemptOutcome::TimedOut {
                                send_to_ack_ms: *seq_ms,
                                method,
                                acked_url: seq_url.clone(),
                            }));
                        }
                    }
                }
            }

            let included = results.iter().find_map(|r| match r.as_ref().ok() {
                Some(SendAttemptOutcome::Included { receipt, send_to_ack_ms, dispatch_to_inclusion_ms, method, acked_url }) => {
                    Some((receipt, *send_to_ack_ms, *dispatch_to_inclusion_ms, *method, acked_url.clone()))
                }
                _ => None,
            });

            if let Some((receipt, backup_ack_ms, dispatch_to_inclusion_ms, method, backup_url)) = included {
                let tx_hash = receipt.transaction_hash;
                if tx_hash != expected_hash {
                    error!(%address, %tx_hash, %expected_hash, "broadcast tx hash does not match the one computed at prepare time");
                }

                let (send_to_ack_ms, acked_url) = prefer_sequencer_ack(
                    sequencer_ack.clone(),
                    Some((backup_ack_ms, backup_url)),
                ).expect("backup ack is Some");
                info!(
                    url = %config::redact_rpc_url(&acked_url),
                    send_to_ack_ms,
                    "using this URL's ack for send_to_ack_ms"
                );

                let ack_source = Some(classify_ack_source(&sequencer_url, &acked_url));
                let acked_url_redacted = Some(config::redact_rpc_url(&acked_url));

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
                        ack_source,
                        acked_url: acked_url_redacted,
                    });
                } else {
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
                        ack_source,
                        acked_url: acked_url_redacted,
                    });
                }
            } else if let Some((backup_ack_ms, method, backup_url)) = results.iter().find_map(|r| match r.as_ref().ok() {
                Some(SendAttemptOutcome::TimedOut { send_to_ack_ms, method, acked_url }) => {
                    Some((*send_to_ack_ms, *method, acked_url.clone()))
                }
                _ => None,
            }) {
                let (send_to_ack_ms, acked_url) = prefer_sequencer_ack(
                    sequencer_ack.clone(),
                    Some((backup_ack_ms, backup_url)),
                ).expect("backup ack is Some");
                info!(
                    url = %config::redact_rpc_url(&acked_url),
                    send_to_ack_ms,
                    "using this URL's ack for send_to_ack_ms"
                );
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
                    dispatch_to_inclusion_ms: None,
                    ack_source: Some(classify_ack_source(&sequencer_url, &acked_url)),
                    acked_url: Some(config::redact_rpc_url(&acked_url)),
                });
            } else if let Some((send_to_ack_ms, acked_url)) = sequencer_ack {
                // P0 follow-up 18b — this branch should no longer be
                // reachable in practice: whenever sequencer_ack is Some, the
                // `providers.first().or(sequencer.as_ref())` fallback above
                // always finds a provider to poll with (the sequencer
                // connection itself, at minimum), so an Included or
                // TimedOut result always gets pushed into `results` before
                // this `else if` is even reached. Kept as defense-in-depth
                // for a future refactor of the block above, not because
                // this path is expected to fire.
                info!(
                    url = %config::redact_rpc_url(&acked_url),
                    send_to_ack_ms,
                    "sequencer acked but inclusion was not confirmed on a backup RPC"
                );
                let detail = format!(
                    "sequencer acked send_raw_transaction ({}) but inclusion was not confirmed — \
                     tx {expected_hash:#x} may still be pending",
                    config::redact_rpc_url(&acked_url)
                );
                tracing::warn!(%address, tx_hash = %expected_hash, timeout_ms = inclusion_timeout_ms, "sequencer ack without inclusion confirmation");
                let _ = bus.send(ServerEvent::MintResult {
                    address: format!("{address:#x}"),
                    success: false,
                    detail,
                    trigger_to_dispatch_ms: Some(trigger_to_dispatch_ms),
                    prepare_age_ms,
                    send_to_ack_ms: Some(send_to_ack_ms),
                    dispatch_to_inclusion_ms: None,
                    // This branch only reaches here when acked_url IS the
                    // sequencer (see the P0 follow-up 18b comment above —
                    // sequencer_ack is the only source in scope), so
                    // classify_ack_source isn't needed to know the answer.
                    ack_source: Some("sequencer"),
                    acked_url: Some(config::redact_rpc_url(&acked_url)),
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
                let _ = bus.send(ServerEvent::MintResult {
                    address: format!("{address:#x}"),
                    success: false,
                    detail,
                    trigger_to_dispatch_ms: Some(trigger_to_dispatch_ms),
                    prepare_age_ms,
                    send_to_ack_ms: None,
                    dispatch_to_inclusion_ms: None,
                    // Nothing ever acked — there is no path to attribute.
                    ack_source: None,
                    acked_url: None,
                });
            }
        })));
    }

    await_all_and_report_panics(handles, bus).await;
    Ok(())
}

/// STEP 33 — `join_all(handles).await` (the prior form here) discarded
/// each task's `JoinError` outright. A panic inside a per-wallet task
/// (e.g. one of the `prefer_sequencer_ack(...).expect(...)` calls above,
/// or any future one) previously ended that wallet's fire with NO log
/// line and NO `MintResult` at all — completely indistinguishable, from
/// the outside, from the task never having run at all. Extracted into
/// its own function (rather than left inline) specifically so this
/// behavior is independently unit-testable against a handle that
/// genuinely panics, not just inspectable by reading the code. Every
/// handle's outcome is checked explicitly: a panic is loud (a logged
/// `error!` plus a real `MintResult`) instead of silent, closing the
/// "every fire-path outcome must be a result or a logged error, no
/// exceptions" gap this bug exposed.
async fn await_all_and_report_panics(handles: Vec<(Address, tokio::task::JoinHandle<()>)>, bus: &EventBus) {
    for (address, handle) in handles {
        if let Err(join_err) = handle.await {
            error!(%address, error = %join_err, "wallet's fire task panicked — this is an internal bug, not a network failure");
            let _ = bus.send(ServerEvent::MintResult {
                address: format!("{address:#x}"),
                success: false,
                detail: format!("internal panic during fire (this wallet's task crashed before producing a result): {join_err}"),
                trigger_to_dispatch_ms: None,
                // Not meaningful here — the task panicked before this
                // could be computed reliably, and there is no "unknown"
                // representation for this field (unlike the Option<u64>
                // timing fields above). success: false + detail already
                // make this branch unambiguous.
                prepare_age_ms: 0,
                send_to_ack_ms: None,
                dispatch_to_inclusion_ms: None,
                ack_source: None,
                acked_url: None,
            });
        }
    }
}

fn apply_pct_jitter(value: u128, pct: i64) -> u128 {
    let delta = (value as i128 * pct as i128) / 100;
    (value as i128 + delta).max(0) as u128
}

#[cfg(test)]
mod tests {
    use super::{
        apply_pct_jitter, await_all_and_report_panics, classify_ack_source, fire_prepared, sample_fire_jitter_ms,
        send_raw_tx_with_timeout, timed_read, warm_sequencer, HttpProvider, PreparedWallet, SendAttempt,
    };
    use crate::bus;
    use alloy::consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy::primitives::{Address, Log, TxHash};
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::rpc::types::TransactionReceipt;
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::Arc;

    /// A minimal JSON-RPC mock that answers `eth_sendRawTransaction` with a
    /// fixed hash and `eth_getTransactionReceipt` with an immediate,
    /// successful receipt for that same hash — enough for `fire_prepared`
    /// to exercise a real send + real inclusion-poll round trip without a
    /// live chain. Not a general-purpose mock: any other method is a bug
    /// in the test, not something to shrug off, so it panics loudly.
    async fn spawn_mock_sequencer(tx_hash: TxHash) -> String {
        let receipt = TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(ReceiptWithBloom {
                receipt: Receipt::<Log> { status: Eip658Value::success(), cumulative_gas_used: 21_000, logs: vec![] },
                logs_bloom: Default::default(),
            }),
            transaction_hash: tx_hash,
            transaction_index: Some(0),
            block_hash: Some(Default::default()),
            block_number: Some(1),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        };
        let receipt_json = serde_json::to_value(&receipt).expect("TransactionReceipt must serialize");

        let handler = move |Json(body): Json<Value>| {
            let receipt_json = receipt_json.clone();
            async move {
                let id = body["id"].clone();
                let result = match body["method"].as_str() {
                    Some("eth_sendRawTransaction") => json!(format!("{tx_hash:#x}")),
                    Some("eth_getTransactionReceipt") => receipt_json,
                    other => panic!("mock sequencer got an unexpected RPC method: {other:?}"),
                };
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
        };
        let app = Router::new().route("/", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/")
    }

    /// P0 follow-up 18b regression test — the exact false-negative this
    /// closes: `sequencer_http_url` acks the broadcast, no backup RPC is
    /// configured at all (`http_rpc_urls` empty, the shape a sequencer-only
    /// race_mode config produces), and inclusion confirmation must now come
    /// from the sequencer's own warmed connection instead of reporting
    /// `success: false` for a tx that actually landed.
    #[tokio::test]
    async fn sequencer_ack_only_with_no_backup_rpc_resolves_to_real_inclusion() {
        let tx_hash = TxHash::from([0x11u8; 32]);
        let mock_url = spawn_mock_sequencer(tx_hash).await;

        let sequencer: HttpProvider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(mock_url.parse().unwrap());

        let mut cfg = crate::config::test_config();
        cfg.http_rpc_urls = vec![];
        cfg.sequencer_http_url = mock_url;
        cfg.jitter_ms_min = 0;
        cfg.jitter_ms_max = 0;
        cfg.block_time_ms = 20;
        cfg.inclusion_timeout_ms = 2_000;

        let prepared = vec![PreparedWallet {
            address: Address::ZERO,
            raw_tx: Arc::from(vec![0xde, 0xad, 0xbe, 0xef]),
            tx_hash,
            prepared_at: std::time::Instant::now(),
        }];

        let bus = bus::new_bus();
        let mut rx = bus.subscribe();

        fire_prepared(&cfg, &prepared, &[], Some(&sequencer), &bus, std::time::Instant::now(), None)
            .await
            .expect("fire_prepared itself must not error — providers-empty-but-sequencer-present is a supported shape");

        let mut saw_result = false;
        while let Ok(event) = rx.try_recv() {
            if let bus::ServerEvent::MintResult { success, dispatch_to_inclusion_ms, detail, .. } = event {
                saw_result = true;
                assert!(
                    success,
                    "sequencer-ack-only fire with no working backup RPC must resolve to a real \
                     inclusion result, not a false success: false (detail was: {detail})"
                );
                assert!(
                    dispatch_to_inclusion_ms.is_some(),
                    "a real Included outcome must carry a real dispatch_to_inclusion_ms"
                );
            }
        }
        assert!(saw_result, "fire_prepared must emit a MintResult event for the one prepared wallet");
    }

    #[test]
    fn zero_jitter_is_a_noop() {
        assert_eq!(apply_pct_jitter(1_000_000, 0), 1_000_000);
        assert_eq!(apply_pct_jitter(0, 0), 0);
    }

    #[test]
    fn positive_jitter_increases_by_percent() {
        assert_eq!(apply_pct_jitter(1_000_000, 8), 1_080_000);
        assert_eq!(apply_pct_jitter(1_000_000, 100), 2_000_000);
    }

    #[test]
    fn negative_jitter_decreases_by_percent() {
        assert_eq!(apply_pct_jitter(1_000_000, -8), 920_000);
        assert_eq!(apply_pct_jitter(1_000_000, -100), 0);
    }

    #[test]
    fn negative_jitter_never_produces_a_negative_gas_value() {
        assert_eq!(apply_pct_jitter(1_000_000, -150), 0);
        assert_eq!(apply_pct_jitter(1_000_000, -1_000), 0);
        assert_eq!(apply_pct_jitter(0, -100), 0);
        assert_eq!(apply_pct_jitter(0, -1_000), 0);
    }

    #[test]
    fn jitter_max_zero_skips_sampling_and_sleep() {
        assert_eq!(sample_fire_jitter_ms(0, 0), None);
        assert_eq!(sample_fire_jitter_ms(40, 0), None);
    }

    #[test]
    fn jitter_max_nonzero_samples_in_range() {
        for _ in 0..32 {
            let ms = sample_fire_jitter_ms(40, 400).expect("max > 0 must sample");
            assert!((40..=400).contains(&ms), "sampled {ms} outside 40..=400");
        }
    }

    async fn spawn_mock_write_only_rpc() -> String {
        let handler = |Json(body): Json<Value>| async move {
            let id = body["id"].clone();
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "the method does not exist/is not available"}
            }))
        };
        let app = Router::new().route("/", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/")
    }

    fn last_log_level(rx: &mut tokio::sync::broadcast::Receiver<bus::ServerEvent>) -> Option<String> {
        let mut level = None;
        while let Ok(event) = rx.try_recv() {
            if let bus::ServerEvent::Log { level: l, .. } = event {
                level = Some(l);
            }
        }
        level
    }

    /// P0 follow-up 21b regression test — a write-only endpoint (real
    /// JSON-RPC error response to every method, exactly Robinhood Chain's
    /// real sequencer submit endpoint's shape) must be classified as a
    /// successful warm (`info`), not a warm failure (`warn`): the
    /// connection and handshake genuinely succeeded, the endpoint just
    /// doesn't implement general read methods.
    #[tokio::test]
    async fn warm_sequencer_treats_a_write_only_endpoints_error_response_as_a_successful_warm() {
        let mock_url = spawn_mock_write_only_rpc().await;
        let mut cfg = crate::config::test_config();
        cfg.sequencer_http_url = mock_url;

        let bus = bus::new_bus();
        let mut rx = bus.subscribe();

        let result = warm_sequencer(&cfg, &bus).await;
        assert!(result.is_some(), "warm_sequencer must still return the provider even when the probe errors");

        let level = last_log_level(&mut rx);
        assert_eq!(
            level.as_deref(),
            Some("info"),
            "a JSON-RPC error response (endpoint reachable, method just unimplemented) must log info, not warn"
        );
    }

    /// A genuine transport-level failure (nothing listening at all — no
    /// JSON-RPC error payload is even possible) must still be classified
    /// as a real warm failure (`warn`), so this distinction isn't just
    /// "always claim success."
    #[tokio::test]
    async fn warm_sequencer_still_warns_on_a_genuine_connection_failure() {
        let mut cfg = crate::config::test_config();
        // Port 0 is not a listening address once resolved by connect_http —
        // using an address nothing binds to (a closed/reserved port on
        // loopback) forces a real connection-level failure, not a JSON-RPC
        // error response.
        cfg.sequencer_http_url = "http://127.0.0.1:1/".to_string();

        let bus = bus::new_bus();
        let mut rx = bus.subscribe();

        let result = warm_sequencer(&cfg, &bus).await;
        assert!(result.is_some(), "warm_sequencer must still return the provider even when the probe fails outright");

        let level = last_log_level(&mut rx);
        assert_eq!(
            level.as_deref(),
            Some("warn"),
            "a genuine connection failure (no JSON-RPC error payload possible) must still log warn"
        );
    }

    // --- STEP 28 (final): classify_ack_source ---

    #[test]
    fn classify_ack_source_matches_the_configured_sequencer_exactly() {
        let seq = "https://sequencer.testnet.chain.robinhood.com/".to_string();
        assert_eq!(classify_ack_source(&seq, &seq), "sequencer");
    }

    #[test]
    fn classify_ack_source_is_backup_when_url_differs_from_sequencer() {
        let seq = "https://sequencer.testnet.chain.robinhood.com/".to_string();
        let backup = "https://rpc.testnet.chain.robinhood.com/".to_string();
        assert_eq!(classify_ack_source(&seq, &backup), "backup");
    }

    #[test]
    fn classify_ack_source_is_backup_when_no_sequencer_is_configured() {
        // race_mode off / sequencer_http_url unset — every real ack in
        // this codebase's own Config default is empty string, never used
        // as a comparison target that could spuriously match.
        assert_eq!(classify_ack_source("", "https://rpc.testnet.chain.robinhood.com/"), "backup");
    }

    #[test]
    fn classify_ack_source_does_not_match_on_a_shared_host_alone() {
        // A backup RPC sharing the sequencer's host (e.g. two different
        // paths/ports on the same domain) must NOT be misclassified as
        // the sequencer — exact string equality only, not a host
        // heuristic, per this function's own doc comment.
        let seq = "https://chain.robinhood.com/sequencer".to_string();
        let backup = "https://chain.robinhood.com/rpc".to_string();
        assert_eq!(classify_ack_source(&seq, &backup), "backup");
    }

    // --- STEP 33: silent fire-path hang, root-caused and regression-tested ---

    /// Never responds to any request — simulates a stalled TCP/RPC
    /// connection with no timeout of its own, the exact real-world shape
    /// this step's fix targets (see `SEND_RAW_TX_TIMEOUT`'s doc comment:
    /// `send_raw_transaction(...).await` previously had nothing bounding
    /// it at all). Sleeps far longer than any timeout used against it
    /// below, so the test genuinely exercises "never responds," not
    /// "responds slowly."
    async fn spawn_mock_hanging_rpc() -> String {
        let handler = |Json(_body): Json<Value>| async move {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Json(json!({"jsonrpc": "2.0", "id": 0, "result": "unreachable — timeout should fire first"}))
        };
        let app = Router::new().route("/", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/")
    }

    /// STEP 33 regression test — the actual root-caused bug: nothing in
    /// this codebase bounded `send_raw_transaction(...).await`, so a
    /// stalled connection (this mock: a handler that never responds)
    /// hung the calling future forever, with nothing ever logged. A
    /// short 200ms timeout here (not the real 10s `SEND_RAW_TX_TIMEOUT`
    /// used at fire time) proves the bounding mechanism itself works
    /// without making this test slow.
    #[tokio::test]
    async fn send_raw_tx_with_timeout_never_hangs_on_a_stalled_connection() {
        let mock_url = spawn_mock_hanging_rpc().await;
        let provider: HttpProvider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(mock_url.parse().unwrap());

        let started = std::time::Instant::now();
        let outcome = send_raw_tx_with_timeout(&provider, &[0xde, 0xad, 0xbe, 0xef], std::time::Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        assert!(
            matches!(outcome, SendAttempt::TimedOut),
            "a stalled connection must resolve to SendAttempt::TimedOut, not hang or silently succeed"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "send_raw_tx_with_timeout took {elapsed:?} against a 200ms timeout — it did not actually bound the wait, \
             which is the exact regression this test exists to catch"
        );
    }

    /// STEP 34 regression test — the real, still-live gap step 33's own
    /// fix did NOT cover: `prepare_fire`'s `get_chain_id`/`get_gas_price`/
    /// `estimate_gas` reads had no timeout at all, and this function runs
    /// synchronously inside `control_loop`'s single message loop (no
    /// `tokio::spawn` around it) — a stall here doesn't just hang one
    /// task, it hangs the ENTIRE bot, with zero log output, since
    /// `prepare_fire`'s first log line only fires after all three reads
    /// succeed. A mock RPC that never responds to `eth_chainId` proves
    /// `timed_read` resolves to a loud, "timed out"-labeled Err within a
    /// short 200ms test timeout, not the real 10s `PREPARE_READ_TIMEOUT` —
    /// same "prove the mechanism, don't make the test slow" approach as
    /// step 33's own timeout test above.
    #[tokio::test]
    async fn timed_read_never_hangs_on_a_stalled_provider_call() {
        let mock_url = spawn_mock_hanging_rpc().await;
        let provider: HttpProvider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(mock_url.parse().unwrap());

        let started = std::time::Instant::now();
        let result = timed_read(provider.get_chain_id(), "fetching chain id", std::time::Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        let err = result.expect_err("a stalled read must resolve to an Err, not hang or silently succeed");
        assert!(
            err.to_string().contains("timed out"),
            "the error should clearly say this was a timeout, not some other failure: {err}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "timed_read took {elapsed:?} against a 200ms timeout — it did not actually bound the wait, \
             which is the exact regression (step 34's silent bot-wide hang) this test exists to catch"
        );
    }

    /// STEP 33 regression test — the other real gap the same bug report
    /// surfaced: `join_all(handles).await` used to discard every per-
    /// wallet task's `JoinError`, so a genuine panic inside one wallet's
    /// fire task previously vanished completely — no log, no
    /// `MintResult`, indistinguishable from the task never having run.
    /// `await_all_and_report_panics` must turn that into a loud, logged
    /// failure with a real `MintResult` instead.
    #[tokio::test]
    async fn a_panicking_wallet_task_produces_a_logged_result_not_silence() {
        let bus = bus::new_bus();
        let mut rx = bus.subscribe();

        let handle = tokio::spawn(async {
            panic!("deliberate test panic — simulates a genuine bug inside a per-wallet fire task");
        });

        await_all_and_report_panics(vec![(Address::ZERO, handle)], &bus).await;

        let mut saw_result = false;
        while let Ok(event) = rx.try_recv() {
            if let bus::ServerEvent::MintResult { success, detail, .. } = event {
                saw_result = true;
                assert!(!success, "a panicked wallet task must never be reported as a success");
                assert!(
                    detail.contains("panic"),
                    "the MintResult detail should say this was an internal panic, not a network failure: {detail}"
                );
            }
        }
        assert!(
            saw_result,
            "a panicking wallet task must still produce a MintResult — STEP 33's whole point is no silent failures"
        );
    }
}
