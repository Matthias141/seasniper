// NOTE ON API STABILITY: alloy's and axum's surfaces shift across minor
// versions faster than most crates. This is written against alloy 0.9.x /
// axum 0.7.x from memory, not compiled against a pinned lockfile (no Rust
// toolchain in the environment this was authored in). Before relying on
// this for a real mint: `cargo build`, fix the inevitable handful of
// signature mismatches, and dry-run against a testnet contract with the
// exact same mint-gating logic first.

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
    // Never awaited — runs for the process lifetime, persisting
    // arm/disarm/fire/config-change/mint-result events. See audit.rs's
    // doc comment for why this is a bus subscriber rather than scattered
    // call sites, and why a separate file rather than reusing tracing's
    // stdout output.
    tokio::spawn(audit::run_audit_writer(event_bus.clone(), AUDIT_LOG_PATH.to_string()));

    // admin_watch_target is only used by mempool_watch (see
    // watcher::run_mempool_watcher's doc comment for why it's not always
    // the same as `contract`): in seadrop mode, the admin's visible
    // mempool tx targets the nft contract itself, not the SeaDrop
    // singleton `contract` gets set to below for firing. In custom mode
    // there's no such split — the admin's enabling tx and our mint tx
    // target the same contract.
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

    // Identity DB (step 10) — opened and migrated before the router is
    // built, same "fail loudly at startup rather than lazily on first
    // use" principle as config::Config::load above.
    let identity_db = db::open(IDENTITY_DB_PATH)
        .await
        .context("opening identity DB")?;
    let identity_keys =
        identity::crypto::load_or_create(SESSION_KEY_PATH).context("loading/creating session key material")?;

    // Google Sign-In (step 10c) is opt-in at the config level: an
    // existing config.toml with none of the google_oauth_* fields set
    // still starts up exactly as before, with identity routes returning
    // a clear "not configured" error rather than the whole bot failing
    // to boot. Once configured, a broken setup (bad client id, no
    // network to accounts.google.com) DOES fail startup loudly — a
    // deliberately-enabled identity feature that silently doesn't work
    // is worse than a boot failure that says why.
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

    // WebAuthn (step 10e) piggybacks on the same google_oauth_redirect_url
    // for its rp_id/rp_origin — see identity/webauthn.rs's doc comment —
    // so it's gated on the exact same config presence as Google Sign-In,
    // not a separate flag.
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

    // Background: refresh wallet ETH balances every 15s and push updates
    // over the event bus so the UI's wallet grid stays live without polling.
    tokio::spawn(balance_poll_loop(
        app_state.clone(),
        cfg.http_rpc_urls[0].clone(),
        wallet_addrs,
    ));

    // Background: ping every configured HTTP RPC every 15s and push
    // ServerEvent::RpcHealth so the UI's "LINK OK/NO LINK" pill reflects
    // real per-provider health, not just the browser's own WebSocket
    // connection to the bot (see gap #3 in CLAUDE.md). Same 15s cadence
    // as balance_poll_loop — both are low-stakes background telemetry
    // with no reason to disagree on how often they check in. Doesn't
    // touch wallets or the control channel, so it runs standalone rather
    // than through control_loop, same reasoning as balance_poll_loop.
    tokio::spawn(rpc_health_poll_loop(app_state.clone(), cfg.http_rpc_urls.clone()));

    // Background: copy-mint (step 6) — watches tracked_wallets for SeaDrop
    // mintPublic activity and surfaces/auto-fires opportunities through
    // the same control_tx every other trigger mode uses. Always spawned;
    // no-ops (just sleeps) whenever tracked_wallets is empty. See
    // copymint.rs's doc comment for why this runs independently of the
    // main trigger_mode watcher's armed state rather than through it.
    tokio::spawn(copymint::run_copymint_watcher(app_state.clone()));

    // Background: owns the watcher lifecycle and executes fires. Wallets
    // (with their private key material) live only here and in the executor
    // call it makes — never cross into the API/UI layer.
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

    // 127.0.0.1 above is unchanged by step 10.5 — a Cloudflare Tunnel
    // (cloudflared) reaches IN to this bind address as a local client, it
    // is not a reason to widen the bind itself. See ui/README.md's
    // Cloudflare Tunnel + Access section.
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
                    nonce: 0, // nonce lives with the signer in control_loop, not surfaced per-poll here
                    healthy: true,
                });
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
}

/// Pings each configured HTTP RPC every 15s with a cheap read call
/// (`eth_blockNumber`) and reports round-trip latency + success/failure
/// as `ServerEvent::RpcHealth`. Providers are built once up front and
/// reused for every tick, same reasoning as executor.rs's connection
/// warming — measuring "latency of a fresh TCP/TLS handshake" every 15s
/// would systematically overstate the latency these endpoints actually
/// have once warm, which is the number that matters at fire time.
async fn rpc_health_poll_loop(state: SharedState, http_rpc_urls: Vec<String>) {
    let providers: Vec<(String, executor::HttpProvider)> = http_rpc_urls
        .into_iter()
        .filter_map(|url| match url.parse() {
            Ok(parsed) => Some((url, ProviderBuilder::new().disable_recommended_fillers().connect_http(parsed))),
            Err(e) => {
                // A malformed URL in config is a config error, not an RPC
                // health event — nothing to ping, so nothing to report.
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

/// Spawns a watcher future, and if it ever completes with an `Err`, surfaces
/// that loudly (bus log + auto-disarm) instead of leaving the control panel
/// showing ARMED while the watcher has silently died.
///
/// Found live during the step 5 dry run: `run_state_poll_watcher`'s WS
/// connection failed immediately, but since nothing awaited its
/// `JoinHandle`, the failure was completely invisible — the UI just showed
/// ARMED indefinitely with no watcher actually running. mempool_watch had
/// its own hand-rolled version of this safety net (added in step 4a,
/// motivated by "many public RPCs don't support full mempool
/// subscriptions"); this generalizes it to every watcher, since a WS
/// connection or RPC failure can kill any of them at any time, not just
/// mempool_watch's specific subscribe step — timestamp mode is the only
/// one that doesn't need this, since it makes no RPC connection at all.
///
/// Wrapping the future directly (not spawning a second task to watch a
/// first) means `.abort()` on the returned handle still works exactly as
/// before: aborting cancels whatever this task is currently awaiting,
/// including the inner watcher future, at any depth.
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
            // STEP 17 FOLLOW-UP — `bus::log` is UI-only (see bus.rs::log —
            // it never touches tracing/stdout), so this call alone never
            // reached journalctl. That's how 17b's redacted-URL context on
            // run_state_poll_watcher/run_mempool_watcher's connect_ws
            // failures went missing from the operator's systemd-journal
            // diagnosis on the very next occurrence — the wrapping was
            // correct, but this, the ONLY place that error text was
            // emitted from, was invisible outside the browser UI.
            tracing::error!("watcher exited with an error: {e:#} — disarming");
            bus::log(&event_bus, "error", format!("watcher exited with an error: {e:#} — disarming"));
            let _ = control_tx.send(ControlMsg::Disarm).await;
        }
        result
    })
}

/// How long before a known `timestamp`-mode trigger to run the prepare
/// (sign) phase. Long enough that gas price / gas estimate are still
/// fresh at fire time, short enough that "prepare" and "trigger" stay
/// distinct events a human watching the log can tell apart. Not tuned
/// against real mint data — a few seconds is enough to eliminate the
/// signing+lookup latency from the fire path, which is the actual goal;
/// shaving this further is a real but separate optimization.
const PREPARE_LEAD_SECS: u64 = 5;

/// How often to re-run the prepare (sign) phase while armed in poll_state
/// mode. Unlike timestamp mode, poll_state has no known trigger instant to
/// count down to — arm-to-trigger could be minutes or hours — so a single
/// prepare-at-arm-time goes stale the longer the mint takes to go live.
/// Fixed interval, not adaptive: 30s bounds the worst-case staleness to
/// "still pretty fresh" without needing to reason about how fast gas
/// moves on any given chain.
const POLL_STATE_REPREPARE_INTERVAL_SECS: u64 = 30;

/// Owns wallet signers and the watcher task. All arm/disarm/fire commands
/// from the API funnel through here as a single-writer loop — avoids any
/// question of two fires racing each other from concurrent API calls.
///
/// Also owns the prepare phase (connection warming + pre-signing, see
/// executor.rs): both read wallet signers, so both have to happen here,
/// for the same single-writer reason as firing itself. `Prepare` is a
/// `ControlMsg` for that reason too — it's never sent from api.rs, only
/// self-sent the same way auto-fire already self-sends `FireNow`.
// Called exactly once, from main(), with startup-computed values each
// used throughout the function body — a wrapper struct here would be
// indirection without real clarity benefit for a single call site.
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
    // Startup-computed but not fixed thereafter (step 8a) — SeaDrop's
    // updatePublicDrop can change mintPrice at any time, including
    // flipping a drop from free to paid, between arm and fire. Re-checked
    // on every Prepare that isn't timestamp mode's single lead-time call —
    // see the ControlMsg::Prepare handler below.
    let mut mint_value = mint_value;
    // Runtime-mutable as of step 8b — ControlMsg::SetTarget updates these
    // when the operator swaps the active target via the UI. `contract`
    // (the SeaDrop singleton address in seadrop mode) deliberately stays
    // a fixed local, not `mut`: 8b only swaps the nft_contract within the
    // singleton's mintPublic calls, never the singleton itself.
    let mut admin_watch_target = admin_watch_target;
    let mut mint_calldata = mint_calldata;
    // Both watcher fns (run_timestamp_watcher, run_state_poll_watcher) return
    // anyhow::Result<()>, not (); the handle type has to match whichever the
    // `match` below spawns, and it's the same for either arm.
    let mut watcher_handle: Option<tokio::task::JoinHandle<Result<()>>> = None;
    // The lead-time sleep-then-send-Prepare task for timestamp mode. Tracked
    // separately from watcher_handle so disarming cancels it even though
    // it's not the watcher itself.
    let mut prepare_timer_handle: Option<tokio::task::JoinHandle<()>> = None;
    // poll_state's periodic re-prepare loop (see POLL_STATE_REPREPARE_INTERVAL_SECS).
    // Separate from prepare_timer_handle because the two are mutually
    // exclusive (one mode or the other) but have different shapes — one
    // fires once, this one repeats until aborted.
    let mut reprepare_handle: Option<tokio::task::JoinHandle<()>> = None;
    // Warmed at Arm time (or just-in-time in FireNow's fallback path below);
    // cleared on Disarm and after firing so a stale connection list can
    // never be paired with a fresh prepare, or vice versa.
    let mut warmed_providers: Vec<executor::HttpProvider> = Vec::new();
    let mut prepared_fire: Option<executor::PreparedFire> = None;
    // STEP 14a — established at Arm time, same lifecycle as
    // warmed_providers and for the same reason: establishing a WS
    // subscription takes up to inclusion::establish_block_ticker's own
    // 5s connect ceiling, which MUST happen ahead of the trigger, not at
    // fire time — adding up to 5s of synchronous latency to every single
    // fire would defeat the entire point of this file's prepare/fire
    // split. `None` means the WS path is unavailable (expected in this
    // sandbox specifically, see gap #11/inclusion.rs's own doc comment)
    // and every fire_prepared call falls back to HTTP polling instead —
    // that fallback decision is made once here, not re-attempted per
    // fire. Cleared on Disarm/after-firing alongside warmed_providers so
    // a stale subscription from a previous arm session is never reused
    // across a re-arm.
    let mut block_ticker: Option<inclusion::BlockTicker> = None;

    while let Some(msg) = control_rx.recv().await {
        match msg {
            ControlMsg::Arm => {
                if watcher_handle.is_some() {
                    bus::log(&state.bus, "warn", "already armed, ignoring duplicate arm");
                    continue;
                }
                let cfg = state.config.read().await.clone();

                // Connection warming happens first and synchronously, before
                // the watcher spawns or the UI sees "armed" — so every
                // endpoint has had its TCP/TLS handshake done at least once
                // before we're relying on it, not at broadcast time.
                warmed_providers = executor::warm_connections(&cfg, &state.bus).await;
                // STEP 14a — same "pay this cost now, not at fire time"
                // principle as the line above. See block_ticker's own
                // declaration comment for why this can't be deferred to
                // fire_prepared itself.
                block_ticker = inclusion::establish_block_ticker(&cfg.ws_rpc_url).await;
                // STEP 15d — this is the one line 14a's own doc comment
                // points to as proof PUSH engaged rather than falling back
                // to POLL, but per step 17's finding, `bus::log` alone
                // never reaches `journalctl` (UI-only), and audit.rs's
                // writer explicitly skips `ServerEvent::Log` too (see its
                // own comment: "not what RUNBOOK.md's checklists need a
                // durable record of"). That left this specific line
                // reachable ONLY by watching the live UI at the exact
                // moment of Arm — unusable for after-the-fact verification
                // on a headless VPS. Also emit via `tracing::info!` so
                // `journalctl -u mint-sniper` shows it durably, same
                // pattern as step 17's fix.
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
                        // timestamp mode makes no RPC connection at all — no
                        // supervision needed, it can't fail this way.
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
                            // Fail loudly and fall back rather than silently
                            // watching nothing — this is exactly the trap
                            // the old unconditional fallthrough to
                            // poll_state fell into (selecting mempool_watch
                            // used to silently do poll_state instead, with
                            // no warning at all).
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
                        // Unrecognized trigger_mode string — same fail-
                        // loud-and-fall-back principle as the mempool_watch
                        // misconfiguration case above, rather than the old
                        // silent fallthrough.
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

                // Auto-fire when the watcher's trigger flips, by routing
                // back through the same control channel FireNow handles —
                // keeps auto-fire and manual-fire on one code path.
                let control_tx = state.control_tx.clone();
                tokio::spawn(async move {
                    if trigger_rx.changed().await.is_ok() {
                        let _ = control_tx.send(ControlMsg::FireNow).await;
                    }
                });

                // Schedule the prepare (sign) phase. poll_state can't predict
                // when its own trigger fires, so there's no lead time to
                // wait out — prepare right away, immediately after warming.
                // timestamp mode knows the exact instant, so it waits until
                // PREPARE_LEAD_SECS before that instant, trading a little
                // gas-price freshness for a real "prepare" event distinct
                // from "trigger" in the log.
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

                        // No known trigger instant to count down to, so
                        // instead of a single prepare there's a standing
                        // loop that keeps re-signing (fresh nonce is a
                        // no-op here since nothing was broadcast yet, fresh
                        // gas price is the actual point) for as long as
                        // we're armed. Disarm and FireNow both abort this —
                        // see reprepare_handle's cleanup below.
                        let control_tx = state.control_tx.clone();
                        reprepare_handle = Some(tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(std::time::Duration::from_secs(
                                    POLL_STATE_REPREPARE_INTERVAL_SECS,
                                ))
                                .await;
                                if control_tx.send(ControlMsg::Prepare).await.is_err() {
                                    break; // control_loop is gone
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

                // 8a: re-fetch mintPrice at the same cadence gas already
                // gets refreshed. Skipped for timestamp mode's single
                // prepare-at-T-minus-PREPARE_LEAD_SECS call — that window
                // is short enough that staleness isn't a real concern,
                // same reasoning 3b already applies there to gas. Every
                // other mode's arm-to-trigger window is unbounded, so a
                // price that changed since arming (or since the last
                // periodic re-prepare) would otherwise go unnoticed until
                // the contract reverts on a msg.value mismatch at fire
                // time — step 3c's fix means that reverts report
                // success: false rather than a false positive, but it's
                // still a missed mint this re-check prevents. Only
                // meaningful in seadrop mode — custom mode has no
                // getPublicDrop equivalent to re-check against.
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
                        });
                    }
                    Err(e) => {
                        // {:#} (anyhow's alternate Display), not {} — {}
                        // only prints the outermost .context() frame
                        // ("estimating gas"), hiding the actual RPC/revert
                        // reason underneath it. Confirmed live during the
                        // step 5 dry run: a genuine "execution reverted:
                        // not active" was reduced to an opaque "prepare
                        // failed: estimating gas" with no way to tell from
                        // the event feed alone what was actually wrong.
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
                block_ticker = None; // drops the watch::Sender's clone; the ticker task exits once every receiver is gone
                state.armed.store(false, Ordering::Relaxed);
                let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: false });
                bus::log(&state.bus, "warn", "disarmed");
            }

            ControlMsg::FireNow => {
                // STEP 13e — the earliest point THIS process can call
                // "trigger detected": right as control_loop starts acting
                // on it, whether that's a watcher's auto-trigger (which
                // reaches here via trigger_rx.changed() -> one mpsc send,
                // sub-millisecond in practice, not separately instrumented)
                // or a manual UI "Fire Now" click (where this genuinely IS
                // the detection instant — there is no earlier "trigger" to
                // measure from for a manual fire).
                let trigger_detected_at = std::time::Instant::now();
                let cfg = state.config.read().await.clone();
                bus::log(&state.bus, "warn", "FIRING all wallets");

                let fire_result = if let Some(pf) = prepared_fire.take() {
                    // Committing to broadcast this batch now — this is the
                    // one place next_nonce advances (see prepare_fire's doc
                    // comment for why it doesn't advance on its own).
                    advance_nonces(&mut wallets, &pf.wallets);
                    executor::fire_prepared(&cfg, &pf.wallets, &pf.providers, &state.bus, trigger_detected_at, block_ticker.clone()).await
                } else {
                    // Manual fire (UI "Fire Now") without a prior arm, or a
                    // prepare that failed above — fall back to signing right
                    // here rather than refusing to fire. Slower (this is
                    // exactly the round-trip-in-the-critical-path prepare
                    // exists to avoid), but a working fire beats a fire that
                    // silently no-ops because nothing was pre-signed.
                    bus::log(
                        &state.bus,
                        "warn",
                        "firing without a prior prepare — signing now, this will be slower",
                    );
                    let providers = if warmed_providers.is_empty() {
                        executor::warm_connections(&cfg, &state.bus).await
                    } else {
                        warmed_providers.clone()
                    };
                    // STEP 14a — reuse an already-established arm-time
                    // ticker if one exists (matches warmed_providers'
                    // reuse just above); only pay establish_block_ticker's
                    // own up-to-5s connect cost here, in this
                    // already-known-slower fallback path, if this fire
                    // never went through Arm at all.
                    let ticker_for_fire = if block_ticker.is_some() {
                        block_ticker.clone()
                    } else {
                        inclusion::establish_block_ticker(&cfg.ws_rpc_url).await
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
                            executor::fire_prepared(&cfg, &w, &providers, &state.bus, trigger_detected_at, ticker_for_fire).await
                        }
                        Err(e) => Err(e),
                    }
                };

                if let Err(e) = fire_result {
                    // {:#} for the same reason as the Prepare handler above
                    // — the full context chain, not just the outermost frame.
                    bus::log(&state.bus, "error", format!("fire sequence error: {e:#}"));
                }

                // Disarm after firing — a completed mint attempt shouldn't
                // silently stay "armed" and re-fire on a stale watcher signal.
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
                block_ticker = None;
                state.armed.store(false, Ordering::Relaxed);
                let _ = state.bus.send(bus::ServerEvent::ArmedState { armed: false });
            }

            ControlMsg::FireCopymint { contract: copy_contract, calldata: copy_calldata, value: copy_value } => {
                // Deliberately does NOT touch watcher_handle/armed/
                // warmed_providers/prepared_fire — a copymint fire is
                // independent of the main trigger_mode's arm/disarm
                // lifecycle (see copymint.rs's doc comment: this can fire
                // while the main watcher is armed, disarmed, or doesn't
                // exist at all for this run). Always JIT-signs, same
                // fallback path FireNow uses when nothing was pre-signed
                // — there's no "prepare ahead of time" for copymint since
                // the target contract isn't known until the moment of
                // detection.
                let trigger_detected_at = std::time::Instant::now(); // step 13e — see FireNow's identical note above
                let cfg = state.config.read().await.clone();
                bus::log(&state.bus, "warn", format!("FIRING copymint opportunity: contract {copy_contract:#x}"));

                let providers = if warmed_providers.is_empty() {
                    executor::warm_connections(&cfg, &state.bus).await
                } else {
                    warmed_providers.clone()
                };
                // STEP 14a — same reuse-if-armed, establish-fresh-if-not
                // logic as FireNow's fallback path above.
                let ticker_for_fire = if block_ticker.is_some() {
                    block_ticker.clone()
                } else {
                    inclusion::establish_block_ticker(&cfg.ws_rpc_url).await
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
                        if let Err(e) = executor::fire_prepared(&cfg, &w, &providers, &state.bus, trigger_detected_at, ticker_for_fire).await {
                            bus::log(&state.bus, "error", format!("copymint fire sequence error: {e:#}"));
                        }
                    }
                    Err(e) => bus::log(&state.bus, "error", format!("copymint prepare failed: {e:#}")),
                }
            }

            ControlMsg::SetTarget { nft_contract, mint_calldata: new_calldata, mint_value: new_value } => {
                // Same cleanup discipline Disarm already has (step 3b) —
                // a target swap must not leave an in-flight prepared fire
                // or re-prepare loop signed for the OLD nft_contract's
                // calldata sitting around to be broadcast by mistake.
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
                block_ticker = None; // same conservative "treat like a re-arm" cleanup warmed_providers already gets
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

/// Advances `next_nonce` for exactly the wallets present in `prepared` —
/// called once, right at the point control_loop commits to broadcasting a
/// prepared batch. `prepare_fire` itself never advances the nonce (see its
/// doc comment): it can be called many times before anything is fired,
/// and every one of those calls has to sign the same still-unused nonce,
/// not a fresh one each time.
fn advance_nonces(wallets: &mut [wallet::ManagedWallet], prepared: &[executor::PreparedWallet]) {
    for w in wallets.iter_mut() {
        if prepared.iter().any(|pw| pw.address == w.address) {
            w.next_nonce += 1;
        }
    }
}

/// step 8a: whether a mintPrice change since the last check is alarming
/// enough to log at `warn` (vs a quieter `info`) in the
/// `ControlMsg::Prepare` handler. Threshold is >2x the old value — loud
/// enough to catch a real, meaningful price change without firing on
/// gas-price-style noise (there isn't any here; mintPrice only moves when
/// a project deliberately calls `updatePublicDrop`, so any change at all
/// is real, but not every real change is alarming). Free -> paid is
/// already covered by this same rule, not a special case: `old_value * 2
/// == 0` when `old_value == 0`, so any nonzero `new_value` already
/// exceeds it.
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
        // The specific case 8a's task description calls out by name —
        // covered by the general >2x rule, not a special case: 0 * 2 == 0.
        assert!(is_alarming_price_increase(U256::ZERO, U256::from(1u64)));
    }

    #[test]
    fn just_over_2x_is_alarming() {
        assert!(is_alarming_price_increase(U256::from(100u64), U256::from(201u64)));
    }

    #[test]
    fn exactly_2x_is_not_alarming() {
        // Boundary: strictly greater than 2x, not >=, so a project simply
        // doubling their price isn't flagged as loudly as a >2x jump.
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

        // Built independently of encode_mint_calldata's own logic — plain
        // byte concatenation, not a round-trip through the same encoder
        // being tested.
        //
        // Selector: keccak256("mint(uint256)")[..4] = 0xa0712d68 — this is
        // one of the most widely-referenced function selectors in the NFT
        // space (mint(uint256) is an extremely common signature), and it's
        // also cross-checked independently via alloy::primitives::keccak256
        // in a standalone scratch binary (see step 4c's report), not just
        // trusted from memory.
        let mut expected: Vec<u8> = vec![0xa0, 0x71, 0x2d, 0x68];
        expected.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());

        assert_eq!(calldata, expected);
        assert_eq!(calldata.len(), 4 + 32);
    }

    #[test]
    fn encode_mint_calldata_encodes_multiple_args_in_order() {
        // Two uint256 args, to catch an arg-ordering bug that a
        // single-argument test can't — coerce_str/DynSolType encoding
        // could accidentally swap or misalign args and a 1-arg test would
        // still pass.
        let calldata =
            encode_mint_calldata("mint(uint256,uint256)", &["1".to_string(), "2".to_string()])
                .unwrap();

        // keccak256("mint(uint256,uint256)")[..4] = 0x1b2ef1ca, computed and
        // cross-checked independently the same way as the selector above.
        let mut expected: Vec<u8> = vec![0x1b, 0x2e, 0xf1, 0xca];
        expected.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
        expected.extend_from_slice(&U256::from(2u64).to_be_bytes::<32>());

        assert_eq!(calldata, expected);
    }

    #[test]
    fn encode_selector_only_matches_known_selector() {
        // keccak256("mintActive()")[..4] = 0x25fd90f3, cross-checked
        // independently the same way as the selectors above.
        let sel = encode_selector_only("mintActive()").unwrap();
        assert_eq!(sel, vec![0x25, 0xfd, 0x90, 0xf3]);
    }

    /// STEP 17 FOLLOW-UP — a regression guard, not just a one-time manual
    /// check. `grep -rn "WsConnect::new" src/` was run directly against
    /// this exact source tree (not from memory) after the step 17 fix and
    /// found exactly these four call sites, each already wrapped in
    /// step 17b/17-follow-up's redacted-URL error context AND emitting via
    /// both `tracing::error!`/`warn!` (journalctl-visible) and `bus::log`
    /// (UI-visible) on failure — see this session's CLAUDE.md step 17
    /// section for why both were needed, not just one. If this test ever
    /// fails, a new WS connection call site was added somewhere in `src/`
    /// that this audit didn't cover — find it with the same grep and give
    /// it the same three things (redacted-URL context, tracing emission,
    /// bus::log emission) before updating this list.
    #[test]
    fn every_ws_connect_call_site_is_accounted_for() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let expected_files = [
            "copymint.rs",
            "inclusion.rs",
            // watcher.rs has two independent call sites
            // (run_state_poll_watcher, run_mempool_watcher) — both counted.
            "watcher.rs",
        ];
        const EXPECTED_TOTAL_CALL_SITES: usize = 4;

        // Needle requires assignment context (`= WsConnect::new(`), which
        // every real call site has (`let ws` followed by that pattern)
        // but a doc-comment mention like config.rs's backtick-quoted
        // reference to the type does not.
        let needle = ["= ", "WsConnect", "::new("].concat();

        let mut found_files: Vec<String> = Vec::new();
        let mut total = 0usize;
        for entry in std::fs::read_dir(&src_dir).expect("reading src/ dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
            // Skip this test's own file — main.rs is confirmed (by the
            // same manual grep this test encodes) to have no real call
            // site, but DOES discuss the exact needle text in this test's
            // own doc comment, which would otherwise self-match.
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
