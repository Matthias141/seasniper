// NOTE ON API STABILITY: alloy's and axum's surfaces shift across minor
// versions faster than most crates. This is written against alloy 0.9.x /
// axum 0.7.x from memory, not compiled against a pinned lockfile (no Rust
// toolchain in the environment this was authored in). Before relying on
// this for a real mint: `cargo build`, fix the inevitable handful of
// signature mismatches, and dry-run against a testnet contract with the
// exact same mint-gating logic first.

mod api;
mod bus;
mod config;
mod executor;
mod seadrop;
mod state;
mod wallet;
mod watcher;

use alloy::dyn_abi::{DynSolValue, JsonAbiExt};
use alloy::json_abi::Function;
use alloy::primitives::utils::format_units;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{Context, Result};
use state::{AppState, ControlMsg, SharedState, WalletStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, watch, RwLock};
use tracing::info;

const CONFIG_PATH: &str = "config.toml";
const API_BIND_ADDR: &str = "127.0.0.1:4117";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cfg = config::Config::load(CONFIG_PATH).context("loading config.toml")?;
    let event_bus = bus::new_bus();

    let (contract, mint_calldata, mint_value): (Address, Vec<u8>, U256) = match cfg.mint_mode.as_str()
    {
        "seadrop" => {
            let seadrop_addr: Address = if cfg.seadrop_address.is_empty() {
                seadrop::SEADROP_1_0_MAINNET.parse()?
            } else {
                cfg.seadrop_address.parse()?
            };
            let nft_contract: Address = cfg
                .nft_contract
                .parse()
                .context("bad nft_contract address (required for mint_mode = seadrop)")?;
            let fee_recipient: Address = cfg
                .fee_recipient
                .parse()
                .context("bad fee_recipient address (required for mint_mode = seadrop)")?;

            let drop = seadrop::fetch_public_drop(&cfg.http_rpc_urls[0], seadrop_addr, nft_contract)
                .await
                .context("fetching SeaDrop public drop info — check nft_contract and chain")?;

            info!(
                mint_price_wei = %drop.mint_price_wei,
                start_time = drop.start_time,
                end_time = drop.end_time,
                max_per_wallet = drop.max_per_wallet,
                restrict_fee_recipients = drop.restrict_fee_recipients,
                "seadrop: public drop info fetched"
            );
            bus::log(
                &event_bus,
                "info",
                format!(
                    "seadrop drop found — price {} wei/token, start {}, maxPerWallet {}",
                    drop.mint_price_wei, drop.start_time, drop.max_per_wallet
                ),
            );

            if drop.restrict_fee_recipients {
                let allowed = seadrop::is_fee_recipient_allowed(
                    &cfg.http_rpc_urls[0],
                    seadrop_addr,
                    nft_contract,
                    fee_recipient,
                )
                .await?;
                if !allowed {
                    anyhow::bail!(
                        "fee_recipient {fee_recipient:#x} is not on this drop's allowed list \
                         (restrictFeeRecipients=true) — every mint tx will revert with \
                         FeeRecipientNotAllowed. Find the project's approved fee recipient \
                         (often OpenSea's official address) before arming."
                    );
                }
            }

            if cfg.quantity_per_wallet as u128 > drop.max_per_wallet as u128 {
                bus::log(
                    &event_bus,
                    "warn",
                    format!(
                        "quantity_per_wallet ({}) exceeds this drop's maxTotalMintableByWallet ({}) — mints will revert",
                        cfg.quantity_per_wallet, drop.max_per_wallet
                    ),
                );
            }

            let calldata =
                seadrop::encode_mint_public(nft_contract, fee_recipient, cfg.quantity_per_wallet)?;
            let value = drop.mint_price_wei * U256::from(cfg.quantity_per_wallet);

            // SeaDrop exposes startTime on-chain, well ahead of the drop —
            // no reason to guess a timestamp by hand or fall back to
            // state-polling when the exact second is already known.
            if cfg.trigger_timestamp_unix == 0 {
                cfg.trigger_mode = "timestamp".to_string();
                cfg.trigger_timestamp_unix = drop.start_time;
                bus::log(
                    &event_bus,
                    "info",
                    format!("trigger_timestamp_unix auto-set from getPublicDrop: {}", drop.start_time),
                );
            }

            (seadrop_addr, calldata, value)
        }
        _ => {
            let contract: Address = cfg
                .contract_address
                .parse()
                .context("bad contract address")?;
            let calldata = encode_mint_calldata(&cfg.mint_fn_signature, &cfg.mint_fn_args_template)?;
            (contract, calldata, U256::ZERO)
        }
    };

    let mint_state_selector = encode_selector_only(&cfg.mint_state_fn_signature).unwrap_or_default();

    let private_keys = cfg.resolve_private_keys()?;
    let wallets = wallet::load_wallets(&private_keys, &cfg.http_rpc_urls[0]).await?;
    info!(count = wallets.len(), "wallets ready");

    let initial_status: Vec<WalletStatus> = wallets
        .iter()
        .map(|w| WalletStatus {
            address: format!("{:#x}", w.address),
            balance_eth: "0".into(),
            nonce: w.next_nonce,
            healthy: true,
        })
        .collect();
    let wallet_addrs: Vec<Address> = wallets.iter().map(|w| w.address).collect();

    let (control_tx, control_rx) = mpsc::channel::<ControlMsg>(8);

    let app_state: SharedState = Arc::new(AppState {
        config: RwLock::new(cfg.clone()),
        wallet_status: RwLock::new(initial_status),
        armed: AtomicBool::new(false),
        bus: event_bus.clone(),
        control_tx: control_tx.clone(),
        config_path: CONFIG_PATH.to_string(),
    });

    // Background: refresh wallet ETH balances every 15s and push updates
    // over the event bus so the UI's wallet grid stays live without polling.
    tokio::spawn(balance_poll_loop(
        app_state.clone(),
        cfg.http_rpc_urls[0].clone(),
        wallet_addrs,
    ));

    // Background: owns the watcher lifecycle and executes fires. Wallets
    // (with their private key material) live only here and in the executor
    // call it makes — never cross into the API/UI layer.
    tokio::spawn(control_loop(
        app_state.clone(),
        control_rx,
        contract,
        mint_calldata,
        mint_value,
        mint_state_selector,
        wallets,
    ));

    let listener = tokio::net::TcpListener::bind(API_BIND_ADDR).await?;
    info!(addr = API_BIND_ADDR, "control API listening");
    bus::log(&event_bus, "info", format!("control API listening on {API_BIND_ADDR}"));

    axum::serve(listener, api::router(app_state)).await?;
    Ok(())
}

async fn balance_poll_loop(state: SharedState, http_url: String, addrs: Vec<Address>) {
    let provider = ProviderBuilder::new().on_http(http_url.parse().unwrap());
    loop {
        for addr in &addrs {
            if let Ok(balance) = provider.get_balance(*addr).await {
                let eth_str = format_units(balance, "ether").unwrap_or_else(|_| "?".into());
                let mut list = state.wallet_status.write().await;
                if let Some(entry) = list.iter_mut().find(|w| w.address == format!("{addr:#x}")) {
                    entry.balance_eth = eth_str.clone();
                }
                drop(list);
                let _ = state.bus.send(bus::ServerEvent::WalletUpdate {
                    address: format!("{addr:#x}"),
                    balance_eth: eth_str,
                    nonce: 0, // nonce lives with the signer in control_loop, not surfaced per-poll here
                    healthy: true,
                });
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
}

/// Owns wallet signers and the watcher task. All arm/disarm/fire commands
/// from the API funnel through here as a single-writer loop — avoids any
/// question of two fires racing each other from concurrent API calls.
async fn control_loop(
    state: SharedState,
    mut control_rx: mpsc::Receiver<ControlMsg>,
    contract: Address,
    mint_calldata: Vec<u8>,
    mint_value: U256,
    mint_state_selector: Vec<u8>,
    mut wallets: Vec<wallet::ManagedWallet>,
) {
    // Both watcher fns (run_timestamp_watcher, run_state_poll_watcher) return
    // anyhow::Result<()>, not (); the handle type has to match whichever the
    // `match` below spawns, and it's the same for either arm.
    let mut watcher_handle: Option<tokio::task::JoinHandle<Result<()>>> = None;

    while let Some(msg) = control_rx.recv().await {
        match msg {
            ControlMsg::Arm => {
                if watcher_handle.is_some() {
                    bus::log(&state.bus, "warn", "already armed, ignoring duplicate arm");
                    continue;
                }
                let cfg = state.config.read().await.clone();
                state.armed.store(true, Ordering::Relaxed);
                let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: true });

                let (trigger_tx, mut trigger_rx) = watch::channel(false);
                let bus_clone = state.bus.clone();

                let handle = match cfg.trigger_mode.as_str() {
                    "timestamp" => {
                        let target = cfg.trigger_timestamp_unix;
                        tokio::spawn(watcher::run_timestamp_watcher(target, trigger_tx))
                    }
                    _ => {
                        // default to poll_state; mempool_watch not implemented
                        // in this skeleton (see README).
                        tokio::spawn(watcher::run_state_poll_watcher(
                            cfg.ws_rpc_url.clone(),
                            contract,
                            mint_state_selector.clone(),
                            trigger_tx,
                            bus_clone,
                        ))
                    }
                };
                watcher_handle = Some(handle);

                // Auto-fire when the watcher's trigger flips, by routing
                // back through the same control channel FireNow handles —
                // keeps auto-fire and manual-fire on one code path.
                let control_tx = state.control_tx.clone();
                tokio::spawn(async move {
                    if trigger_rx.changed().await.is_ok() {
                        let _ = control_tx.send(ControlMsg::FireNow).await;
                    }
                });

                bus::log(&state.bus, "info", format!("armed, mode={}", cfg.trigger_mode));
            }

            ControlMsg::Disarm => {
                if let Some(h) = watcher_handle.take() {
                    h.abort();
                }
                state.armed.store(false, Ordering::Relaxed);
                let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: false });
                bus::log(&state.bus, "warn", "disarmed");
            }

            ControlMsg::FireNow => {
                let cfg = state.config.read().await.clone();
                bus::log(&state.bus, "warn", "FIRING all wallets");
                if let Err(e) = executor::fire_all_wallets(
                    &cfg,
                    &mut wallets,
                    contract,
                    mint_calldata.clone(),
                    mint_value,
                    &state.bus,
                )
                .await
                {
                    bus::log(&state.bus, "error", format!("fire sequence error: {e}"));
                }
                // Disarm after firing — a completed mint attempt shouldn't
                // silently stay "armed" and re-fire on a stale watcher signal.
                if let Some(h) = watcher_handle.take() {
                    h.abort();
                }
                state.armed.store(false, Ordering::Relaxed);
                let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: false });
            }
        }
    }
}

fn encode_mint_calldata(signature: &str, args: &[String]) -> Result<Vec<u8>> {
    let func = Function::parse(signature).context("parsing mint fn signature")?;
    let values: Vec<DynSolValue> = func
        .inputs
        .iter()
        .zip(args.iter())
        .map(|(param, arg)| {
            param
                .selector_type()
                .parse::<alloy::dyn_abi::DynSolType>()
                .context("parsing param type")?
                .coerce_str(arg)
                .context("coercing arg into abi value")
        })
        .collect::<Result<_>>()?;
    Ok(func.abi_encode_input(&values)?)
}

fn encode_selector_only(signature: &str) -> Result<Vec<u8>> {
    let func = Function::parse(signature).context("parsing state fn signature")?;
    Ok(func.selector().to_vec())
}
