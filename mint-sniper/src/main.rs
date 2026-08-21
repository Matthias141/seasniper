mod api;
mod audit;
mod auth;
mod bus;
mod config;
mod copymint;
mod db;
mod executor;
mod identity;
mod inclusion;
mod opensea;
mod seadrop;
mod state;
mod target;
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
const TOKEN_PATH: &str = ".sniper-token";
const AUDIT_LOG_PATH: &str = "audit.log";
const API_BIND_ADDR: &str = "127.0.0.1:4117";
const IDENTITY_DB_PATH: &str = "identity.db";
const SESSION_KEY_PATH: &str = ".session-key";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut cfg = config::Config::load(CONFIG_PATH).context("loading config.toml")?;
    let event_bus = bus::new_bus();
    tokio::spawn(audit::run_audit_writer(event_bus.clone(), AUDIT_LOG_PATH.to_string()));

    let (contract, mint_calldata, mint_value, admin_watch_target): (Address, Vec<u8>, U256, Address) =
        match cfg.mint_mode.as_str()
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

            if cfg.trigger_timestamp_unix == 0 {
                cfg.trigger_mode = "timestamp".to_string();
                cfg.trigger_timestamp_unix = drop.start_time;
                bus::log(
                    &event_bus,
                    "info",
                    format!("trigger_timestamp_unix auto-set from getPublicDrop: {}", drop.start_time),
                );
            }

            (seadrop_addr, calldata, value, nft_contract)
        }
        _ => {
            let contract: Address = cfg
                .contract_address
                .parse()
                .context("bad contract address")?;
            let calldata = encode_mint_calldata(&cfg.mint_fn_signature, &cfg.mint_fn_args_template)?;
            (contract, calldata, U256::ZERO, contract)
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

    let api_token = auth::load_or_create_token(TOKEN_PATH).context("loading/creating API token")?;

    let identity_db = db::open(IDENTITY_DB_PATH)
        .await
        .context("opening identity DB")?;
    let identity_keys =
        identity::crypto::load_or_create(SESSION_KEY_PATH).context("loading/creating session key material")?;

    let google_oidc = if !cfg.google_oauth_client_id.is_empty() {
        let secret = cfg
            .resolve_google_oauth_client_secret()
            .context("google_oauth_client_id is set but google_oauth_client_secret_env is unset or unresolvable")?;
        let oidc = identity::oidc::GoogleOidc::discover(
            cfg.google_oauth_client_id.clone(),
            secret,
            cfg.google_oauth_redirect_url.clone(),
        )
        .await
        .context("initializing Google Sign-In")?;
        info!("Google Sign-In configured and discovery succeeded");
        Some(Arc::new(oidc))
    } else {
        info!("Google Sign-In not configured (google_oauth_client_id unset) — /auth/google/* routes will 503");
        None
    };

    let webauthn_state = if !cfg.google_oauth_client_id.is_empty() {
        let ws = identity::webauthn::WebauthnState::new(&cfg.google_oauth_redirect_url, "mint-sniper")
            .context("initializing WebAuthn")?;
        info!("WebAuthn configured (rp_id derived from google_oauth_redirect_url)");
        Some(Arc::new(ws))
    } else {
        None
    };

    let app_state: SharedState = Arc::new(AppState {
        config: RwLock::new(cfg.clone()),
        wallet_status: RwLock::new(initial_status),
        armed: AtomicBool::new(false),
        bus: event_bus.clone(),
        control_tx: control_tx.clone(),
        config_path: CONFIG_PATH.to_string(),
        api_token,
        http_client: reqwest::Client::new(),
        identity_db,
        identity_cookie_key: identity_keys.cookie_key,
        identity_totp_cipher: identity_keys.totp_cipher,
        google_oidc,
        webauthn: webauthn_state,
    });

    tokio::spawn(balance_poll_loop(
        app_state.clone(),
        cfg.http_rpc_urls[0].clone(),
        wallet_addrs,
    ));
    tokio::spawn(rpc_health_poll_loop(app_state.clone(), cfg.http_rpc_urls.clone()));
    tokio::spawn(copymint::run_copymint_watcher(app_state.clone()));
    tokio::spawn(control_loop(
        app_state.clone(),
        control_rx,
        contract,
        admin_watch_target,
        mint_calldata,
        mint_value,
        mint_state_selector,
        wallets,
    ));

    let listener = tokio::net::TcpListener::bind(API_BIND_ADDR).await?;
    info!(addr = API_BIND_ADDR, "control API listening");
    bus::log(&event_bus, "info", format!("control API listening on {API_BIND_ADDR}"));
    axum::serve(listener, api::router(app_state, &cfg.google_oauth_redirect_url)).await?;
    Ok(())
}

async fn balance_poll_loop(state: SharedState, http_url: String, addrs: Vec<Address>) {
    let provider = ProviderBuilder::new().disable_recommended_fillers().connect_http(http_url.parse().unwrap());
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
                    nonce: 0,
                    healthy: true,
                });
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
}

async fn rpc_health_poll_loop(state: SharedState, http_rpc_urls: Vec<String>) {
    let providers: Vec<(String, executor::HttpProvider)> = http_rpc_urls
        .into_iter()
        .filter_map(|url| match url.parse() {
            Ok(parsed) => Some((url, ProviderBuilder::new().disable_recommended_fillers().connect_http(parsed))),
            Err(e) => {
                tracing::warn!(%url, error = %e, "rpc_health_poll_loop: skipping unparseable RPC url");
                None
            }
        })
        .collect();

    loop {
        for (url, provider) in &providers {
            let start = std::time::Instant::now();
            let healthy = provider.get_block_number().await.is_ok();
            let latency_ms = start.elapsed().as_millis() as u64;
            let _ = state.bus.send(bus::ServerEvent::RpcHealth {
                url: url.clone(),
                healthy,
                latency_ms,
            });
        }
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
}

fn spawn_supervised_watcher<F>(
    watcher_fut: F,
    event_bus: bus::EventBus,
    control_tx: mpsc::Sender<ControlMsg>,
) -> tokio::task::JoinHandle<Result<()>>
where
    F: std::future::Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let result = watcher_fut.await;
        if let Err(e) = &result {
            tracing::error!("watcher exited with an error: {e:#} — disarming");
            bus::log(&event_bus, "error", format!("watcher exited with an error: {e:#} — disarming"));
            let _ = control_tx.send(ControlMsg::Disarm).await;
        }
        result
    })
}

const PREPARE_LEAD_SECS: u64 = 5;
const POLL_STATE_REPREPARE_INTERVAL_SECS: u64 = 30;

#[allow(clippy::too_many_arguments)]
async fn control_loop(
    state: SharedState,
    mut control_rx: mpsc::Receiver<ControlMsg>,
    contract: Address,
    admin_watch_target: Address,
    mint_calldata: Vec<u8>,
    mint_value: U256,
    mint_state_selector: Vec<u8>,
    mut wallets: Vec<wallet::ManagedWallet>,
) {
    let mut mint_value = mint_value;
    let mut admin_watch_target = admin_watch_target;
    let mut mint_calldata = mint_calldata;
    let mut watcher_handle: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let mut prepare_timer_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut reprepare_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut warmed_providers: Vec<executor::HttpProvider> = Vec::new();
    let mut warmed_sequencer: Option<executor::HttpProvider> = None;
    let mut prepared_fire: Option<executor::PreparedFire> = None;
    let mut block_ticker: Option<inclusion::BlockTicker> = None;

    while let Some(msg) = control_rx.recv().await {
        match msg {
            ControlMsg::Arm => {
                if watcher_handle.is_some() {
                    bus::log(&state.bus, "warn", "already armed, ignoring duplicate arm");
                    continue;
                }
                let cfg = state.config.read().await.clone();

                warmed_providers = executor::warm_connections(&cfg, &state.bus).await;
                warmed_sequencer = executor::warm_sequencer(&cfg, &state.bus).await;
                // inclusion_ws_url, when set, is the PUSH socket; empty uses ws_rpc_url. Do not point this at the Nitro feed.
                block_ticker = inclusion::establish_block_ticker(cfg.block_ticker_ws_url()).await;
                let inclusion_detection_msg = format!(
                    "inclusion detection: {} for this arm session",
                    if block_ticker.is_some() {
                        "WS push path established"
                    } else {
                        "WS push path unavailable, using HTTP poll fallback"
                    }
                );
                tracing::info!("{inclusion_detection_msg}");
                bus::log(&state.bus, "info", inclusion_detection_msg);
                prepared_fire = None;

                state.armed.store(true, Ordering::Relaxed);
                let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: true });

                let (trigger_tx, mut trigger_rx) = watch::channel(false);
                let bus_clone = state.bus.clone();

                let handle = match cfg.trigger_mode.as_str() {
                    "timestamp" => {
                        let target = cfg.trigger_timestamp_unix;
                        tokio::spawn(watcher::run_timestamp_watcher(target, trigger_tx))
                    }
                    "mempool_watch" => match cfg.mint_enable_admin.trim().parse::<Address>() {
                        Ok(admin) => spawn_supervised_watcher(
                            watcher::run_mempool_watcher(
                                cfg.ws_rpc_url.clone(),
                                admin,
                                admin_watch_target,
                                trigger_tx,
                                bus_clone,
                            ),
                            state.bus.clone(),
                            state.control_tx.clone(),
                        ),
                        Err(e) => {
                            bus::log(
                                &state.bus,
                                "error",
                                format!(
                                    "trigger_mode = \"mempool_watch\" requires mint_enable_admin \
                                     to be a valid address ({e}) — falling back to poll_state"
                                ),
                            );
                            spawn_supervised_watcher(
                                watcher::run_state_poll_watcher(
                                    cfg.ws_rpc_url.clone(),
                                    contract,
                                    mint_state_selector.clone(),
                                    trigger_tx,
                                    bus_clone,
                                ),
                                state.bus.clone(),
                                state.control_tx.clone(),
                            )
                        }
                    },
                    "poll_state" => spawn_supervised_watcher(
                        watcher::run_state_poll_watcher(
                            cfg.ws_rpc_url.clone(),
                            contract,
                            mint_state_selector.clone(),
                            trigger_tx,
                            bus_clone,
                        ),
                        state.bus.clone(),
                        state.control_tx.clone(),
                    ),
                    other => {
                        bus::log(
                            &state.bus,
                            "warn",
                            format!("unrecognized trigger_mode {other:?} — falling back to poll_state"),
                        );
                        spawn_supervised_watcher(
                            watcher::run_state_poll_watcher(
                                cfg.ws_rpc_url.clone(),
                                contract,
                                mint_state_selector.clone(),
                                trigger_tx,
                                bus_clone,
                            ),
                            state.bus.clone(),
                            state.control_tx.clone(),
                        )
                    }
                };
                watcher_handle = Some(handle);

                let control_tx = state.control_tx.clone();
                tokio::spawn(async move {
                    if trigger_rx.changed().await.is_ok() {
                        let _ = control_tx.send(ControlMsg::FireNow).await;
                    }
                });

                match cfg.trigger_mode.as_str() {
                    "timestamp" if cfg.trigger_timestamp_unix > 0 => {
                        let control_tx = state.control_tx.clone();
                        let target = cfg.trigger_timestamp_unix;
                        prepare_timer_handle = Some(tokio::spawn(async move {
                            let now = bus::now_ts();
                            let sleep_secs = target
                                .saturating_sub(now)
                                .saturating_sub(PREPARE_LEAD_SECS);
                            if sleep_secs > 0 {
                                tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                            }
                            let _ = control_tx.send(ControlMsg::Prepare).await;
                        }));
                    }
                    _ => {
                        let _ = state.control_tx.send(ControlMsg::Prepare).await;
                        let control_tx = state.control_tx.clone();
                        reprepare_handle = Some(tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(std::time::Duration::from_secs(
                                    POLL_STATE_REPREPARE_INTERVAL_SECS,
                                ))
                                .await;
                                if control_tx.send(ControlMsg::Prepare).await.is_err() {
                                    break;
                                }
                            }
                        }));
                    }
                }

                bus::log(&state.bus, "info", format!("armed, mode={}", cfg.trigger_mode));
            }

            ControlMsg::Prepare => {
                if warmed_providers.is_empty() {
                    bus::log(
                        &state.bus,
                        "error",
                        "prepare requested but no warmed RPC providers are available",
                    );
                    continue;
                }
                let cfg = state.config.read().await.clone();

                if cfg.mint_mode == "seadrop" && cfg.trigger_mode != "timestamp" {
                    match seadrop::fetch_public_drop(&cfg.http_rpc_urls[0], contract, admin_watch_target).await {
                        Ok(drop) => {
                            let new_value = drop.mint_price_wei * U256::from(cfg.quantity_per_wallet);
                            if new_value != mint_value {
                                let old_eth = format_units(mint_value, "ether").unwrap_or_else(|_| mint_value.to_string());
                                let new_eth = format_units(new_value, "ether").unwrap_or_else(|_| new_value.to_string());
                                if is_alarming_price_increase(mint_value, new_value) {
                                    bus::log(
                                        &state.bus,
                                        "warn",
                                        format!(
                                            "mint price changed {old_eth} -> {new_eth} ETH since last check — re-signing with the new value"
                                        ),
                                    );
                                } else {
                                    bus::log(
                                        &state.bus,
                                        "info",
                                        format!("mint price changed {old_eth} -> {new_eth} ETH since last check"),
                                    );
                                }
                                mint_value = new_value;
                            }
                        }
                        Err(e) => {
                            bus::log(
                                &state.bus,
                                "warn",
                                format!("mint price re-check failed ({e:#}) — using last known value"),
                            );
                        }
                    }
                }

                match executor::prepare_fire(
                    &cfg,
                    &wallets,
                    contract,
                    &mint_calldata,
                    mint_value,
                    &warmed_providers,
                    &state.bus,
                )
                .await
                {
                    Ok(prepared_wallets) => {
                        bus::log(
                            &state.bus,
                            "info",
                            format!("{} wallet(s) pre-signed and ready to fire", prepared_wallets.len()),
                        );
                        prepared_fire = Some(executor::PreparedFire {
                            wallets: prepared_wallets,
                            providers: warmed_providers.clone(),
                            sequencer: warmed_sequencer.clone(),
                        });
                    }
                    Err(e) => {
                        bus::log(&state.bus, "error", format!("prepare failed: {e:#}"));
                    }
                }
            }

            ControlMsg::Disarm => {
                if let Some(h) = watcher_handle.take() {
                    h.abort();
                }
                if let Some(h) = prepare_timer_handle.take() {
                    h.abort();
                }
                if let Some(h) = reprepare_handle.take() {
                    h.abort();
                }
                prepared_fire = None;
                warmed_providers.clear();
                warmed_sequencer = None;
                block_ticker = None;
                state.armed.store(false, Ordering::Relaxed);
                let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: false });
                bus::log(&state.bus, "warn", "disarmed");
            }

            ControlMsg::FireNow => {
                let trigger_detected_at = std::time::Instant::now();
                let cfg = state.config.read().await.clone();
                bus::log(&state.bus, "warn", "FIRING all wallets");

                let fire_result = if let Some(pf) = prepared_fire.take() {
                    advance_nonces(&mut wallets, &pf.wallets);
                    executor::fire_prepared(&cfg, &pf.wallets, &pf.providers, pf.sequencer.as_ref(), &state.bus, trigger_detected_at, block_ticker.clone()).await
                } else {
                    bus::log(
                        &state.bus,
                        "warn",
                        "firing without a prior prepare — signing now, this will be slower",
                    );
                    let (providers, sequencer) = if warmed_providers.is_empty() {
                        (
                            executor::warm_connections(&cfg, &state.bus).await,
                            executor::warm_sequencer(&cfg, &state.bus).await,
                        )
                    } else {
                        (warmed_providers.clone(), warmed_sequencer.clone())
                    };
                    let ticker_for_fire = if block_ticker.is_some() {
                        block_ticker.clone()
                    } else {
                        inclusion::establish_block_ticker(cfg.block_ticker_ws_url()).await
                    };
                    match executor::prepare_fire(
                        &cfg,
                        &wallets,
                        contract,
                        &mint_calldata,
                        mint_value,
                        &providers,
                        &state.bus,
                    )
                    .await
                    {
                        Ok(w) => {
                            advance_nonces(&mut wallets, &w);
                            executor::fire_prepared(&cfg, &w, &providers, sequencer.as_ref(), &state.bus, trigger_detected_at, ticker_for_fire).await
                        }
                        Err(e) => Err(e),
                    }
                };

                if let Err(e) = fire_result {
                    bus::log(&state.bus, "error", format!("fire sequence error: {e:#}"));
                }

                if let Some(h) = watcher_handle.take() {
                    h.abort();
                }
                if let Some(h) = prepare_timer_handle.take() {
                    h.abort();
                }
                if let Some(h) = reprepare_handle.take() {
                    h.abort();
                }
                prepared_fire = None;
                warmed_providers.clear();
                warmed_sequencer = None;
                block_ticker = None;
                state.armed.store(false, Ordering::Relaxed);
                let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: false });
            }

            ControlMsg::FireCopymint { contract: copy_contract, calldata: copy_calldata, value: copy_value } => {
                let trigger_detected_at = std::time::Instant::now();
                let cfg = state.config.read().await.clone();
                bus::log(&state.bus, "warn", format!("FIRING copymint opportunity: contract {copy_contract:#x}"));

                let (providers, sequencer) = if warmed_providers.is_empty() {
                    (
                        executor::warm_connections(&cfg, &state.bus).await,
                        executor::warm_sequencer(&cfg, &state.bus).await,
                    )
                } else {
                    (warmed_providers.clone(), warmed_sequencer.clone())
                };
                let ticker_for_fire = if block_ticker.is_some() {
                    block_ticker.clone()
                } else {
                    inclusion::establish_block_ticker(cfg.block_ticker_ws_url()).await
                };

                match executor::prepare_fire(
                    &cfg,
                    &wallets,
                    copy_contract,
                    &copy_calldata,
                    copy_value,
                    &providers,
                    &state.bus,
                )
                .await
                {
                    Ok(w) => {
                        advance_nonces(&mut wallets, &w);
                        if let Err(e) = executor::fire_prepared(&cfg, &w, &providers, sequencer.as_ref(), &state.bus, trigger_detected_at, ticker_for_fire).await {
                            bus::log(&state.bus, "error", format!("copymint fire sequence error: {e:#}"));
                        }
                    }
                    Err(e) => bus::log(&state.bus, "error", format!("copymint prepare failed: {e:#}")),
                }
            }

            ControlMsg::SetTarget { nft_contract, mint_calldata: new_calldata, mint_value: new_value } => {
                if let Some(h) = watcher_handle.take() {
                    h.abort();
                }
                if let Some(h) = prepare_timer_handle.take() {
                    h.abort();
                }
                if let Some(h) = reprepare_handle.take() {
                    h.abort();
                }
                prepared_fire = None;
                warmed_providers.clear();
                warmed_sequencer = None;
                block_ticker = None;
                let was_armed = state.armed.swap(false, Ordering::Relaxed);
                if was_armed {
                    let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: false });
                }

                admin_watch_target = nft_contract;
                mint_calldata = new_calldata;
                mint_value = new_value;

                bus::log(
                    &state.bus,
                    "info",
                    format!(
                        "active target set to {nft_contract:#x}{}",
                        if was_armed { " — disarmed (re-arm to watch the new target)" } else { "" }
                    ),
                );
            }
        }
    }
}

fn advance_nonces(wallets: &mut [wallet::ManagedWallet], prepared: &[executor::PreparedWallet]) {
    for w in wallets.iter_mut() {
        if prepared.iter().any(|pw| pw.address == w.address) {
            w.next_nonce += 1;
        }
    }
}

fn is_alarming_price_increase(old_value: U256, new_value: U256) -> bool {
    new_value > old_value.saturating_mul(U256::from(2))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_to_paid_is_alarming() {
        assert!(is_alarming_price_increase(U256::ZERO, U256::from(1u64)));
    }

    #[test]
    fn just_over_2x_is_alarming() {
        assert!(is_alarming_price_increase(U256::from(100u64), U256::from(201u64)));
    }

    #[test]
    fn exactly_2x_is_not_alarming() {
        assert!(!is_alarming_price_increase(U256::from(100u64), U256::from(200u64)));
    }

    #[test]
    fn small_increase_is_not_alarming() {
        assert!(!is_alarming_price_increase(U256::from(100u64), U256::from(150u64)));
    }

    #[test]
    fn decrease_is_never_alarming() {
        assert!(!is_alarming_price_increase(U256::from(100u64), U256::from(1u64)));
        assert!(!is_alarming_price_increase(U256::from(100u64), U256::ZERO));
    }

    #[test]
    fn no_change_is_not_alarming() {
        assert!(!is_alarming_price_increase(U256::ZERO, U256::ZERO));
        assert!(!is_alarming_price_increase(U256::from(100u64), U256::from(100u64)));
    }

    #[test]
    fn encode_mint_calldata_matches_known_good_calldata() {
        let calldata = encode_mint_calldata("mint(uint256)", &["1".to_string()]).unwrap();
        let mut expected: Vec<u8> = vec![0xa0, 0x71, 0x2d, 0x68];
        expected.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
        assert_eq!(calldata, expected);
        assert_eq!(calldata.len(), 4 + 32);
    }

    #[test]
    fn encode_mint_calldata_encodes_multiple_args_in_order() {
        let calldata =
            encode_mint_calldata("mint(uint256,uint256)", &["1".to_string(), "2".to_string()])
                .unwrap();
        let mut expected: Vec<u8> = vec![0x1b, 0x2e, 0xf1, 0xca];
        expected.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
        expected.extend_from_slice(&U256::from(2u64).to_be_bytes::<32>());
        assert_eq!(calldata, expected);
    }

    #[test]
    fn encode_selector_only_matches_known_selector() {
        let sel = encode_selector_only("mintActive()").unwrap();
        assert_eq!(sel, vec![0x25, 0xfd, 0x90, 0xf3]);
    }

    #[test]
    fn every_ws_connect_call_site_is_accounted_for() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let expected_files = [
            "copymint.rs",
            "inclusion.rs",
            "watcher.rs",
        ];
        const EXPECTED_TOTAL_CALL_SITES: usize = 4;

        let needle = ["= ", "WsConnect", "::new("].concat();

        let mut found_files: Vec<String> = Vec::new();
        let mut total = 0usize;
        for entry in std::fs::read_dir(&src_dir).expect("reading src/ dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
            if file_name == "main.rs" {
                continue;
            }
            let contents = std::fs::read_to_string(&path).expect("reading source file");
            let count = contents.matches(needle.as_str()).count();
            if count > 0 {
                total += count;
                found_files.push(file_name);
            }
        }
        found_files.sort();

        let mut expected_sorted: Vec<String> = expected_files.iter().map(|s| s.to_string()).collect();
        expected_sorted.sort();

        assert_eq!(
            found_files, expected_sorted,
            "the set of files opening a WS connection changed — audit the new/removed \
             file(s) against CLAUDE.md's step 17 section before updating this list"
        );
        assert_eq!(
            total, EXPECTED_TOTAL_CALL_SITES,
            "the total count of WsConnect::new(...) call sites changed (found {total}, \
             expected {EXPECTED_TOTAL_CALL_SITES}) — a call site was added or removed \
             within an already-known file; audit it the same way"
        );
    }
}
