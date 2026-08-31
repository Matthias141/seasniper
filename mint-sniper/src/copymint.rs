//! Copy-mint (step 6): watch a list of "tracked" wallets and, when one of
//! them mints from a SeaDrop public-stage drop, mint the same collection
//! with our own wallets. Same idea as a Solana copy-trading bot, applied
//! to mints instead of trades.
//!
//! SCOPE — SeaDrop `mintPublic` only, deliberately, not "replay arbitrary
//! calldata to whatever contract a tracked wallet touches, with a
//! different signer." That's dangerous in a way that isn't obvious at
//! first glance: ordinary `mint()` calldata often bakes the caller's own
//! address in as the recipient (e.g. `mint(address to, uint256 qty)`) —
//! replaying that byte-for-byte with OUR signer would mint the NFT to
//! THEM while WE pay gas and price. SeaDrop's `mintPublic` doesn't have
//! this problem: it takes an explicit `minterIfNotPayer` argument, and
//! per `seadrop.rs`'s own doc comment on `encode_mint_public`,
//! `Address::ZERO` there means "mint to whoever pays" — so this module
//! never replays a tracked wallet's calldata at all. It only ever needs
//! to know WHICH `nftContract` (and `feeRecipient`) they're minting; from
//! there it builds and signs an entirely fresh `mintPublic` call, with
//! OUR `minterIfNotPayer = Address::ZERO`, OUR quantity (from config, not
//! copied from the tracked wallet), and OUR wallets as signer, payer, and
//! recipient. `decode_mint_public_call` below intentionally decodes only
//! `nftContract`/`feeRecipient` (args 0/1) and never even looks at
//! `minterIfNotPayer`/`quantity` (args 2/3) — see the
//! `copymint_never_propagates_tracked_wallets_own_minter_choice` test for
//! a direct, executable proof of this, not just an assertion in prose.
//!
//! LIFECYCLE — unlike `watcher.rs`'s `poll_state`/`timestamp`/
//! `mempool_watch`, copymint isn't part of the single-target arm/disarm
//! state machine. Those three all watch for ONE specific, human-
//! configured contract; copymint watches for tracked-wallet activity
//! against ANY SeaDrop collection, which is a different, always-on kind
//! of thing — it runs for the whole process lifetime once
//! `tracked_wallets` is non-empty, the same way `balance_poll_loop`/
//! `rpc_health_poll_loop` in `main.rs` do, not gated by whether the main
//! `trigger_mode` watcher is armed. That's also why this lives in its own
//! module rather than inside `watcher.rs`: different lifecycle, different
//! concerns (ABI-decoding a tracked wallet's calldata, cross-referencing
//! `seadrop.rs`'s `getPublicDrop`, and the free/paid firing-decision
//! logic below are all specific to copymint, not shared with the single-
//! target watchers).
//!
//! SAFETY — free vs paid split, not one auto-fire toggle. A free mint's
//! downside is bounded to gas (already capped by
//! `max_priority_fee_gwei_cap`); a paid mint spends real ETH on a
//! contract nobody configured or looked at in advance, unlike every other
//! trigger mode. `should_auto_fire` below is the actual enforcement: its
//! signature does not even accept `copymint_auto_fire_paid` as a
//! parameter, so there is no code path in this file that can auto-fire a
//! paid opportunity regardless of that config value. `handle_candidate`
//! sends `ControlMsg::FireCopymint` from exactly one place, gated by
//! `should_auto_fire`. The only other place that message is ever sent is
//! `api.rs`'s `/api/copymint/fire` route — an authenticated action gated
//! by a human clicking a UI button, which itself never reads
//! `copymint_auto_fire_paid` either (see that route's doc comment for
//! why). `copymint_auto_fire_paid` exists purely to tell the UI whether
//! to render/enable that button for a given opportunity — it never
//! reaches this file or `main.rs`'s `control_loop` at all (confirmed by
//! grep: `copymint_auto_fire_paid` appears only in `config.rs`'s
//! definition and `api.rs`'s route/UI-facing surface).
//!
//! KNOWN LIMITATION — inherits `CLAUDE.md`'s gap #11 in full.
//! `subscribe_full_pending_transactions` (this module's entire detection
//! mechanism, same call `watcher.rs`'s `run_mempool_watcher` uses) has
//! never been successfully fire-tested end-to-end against a live chain in
//! this sandbox: alloy's WS transport fails TLS validation against the
//! sandbox's intercepting proxy (confirmed as recently as step 9e —
//! structural, not intermittent). This module's parsing/filtering/
//! decoding/firing-decision logic is unit-tested against synthetic
//! pending-tx-shaped data (see the tests below), but has NOT been
//! live-fired from this environment. It needs real testnet validation —
//! tracked-wallet detection actually firing, an opportunity actually
//! surfacing in the UI, manual fire actually working — before it's
//! trustworthy for a real drop. See `CLAUDE.md`'s "copymint" section.

use crate::bus::{self, ServerEvent};
use crate::config::Config;
use crate::seadrop;
use crate::state::{ControlMsg, SharedState};
use alloy::dyn_abi::JsonAbiExt;
use alloy::json_abi::Function;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::TransactionTrait;
use anyhow::{Context, Result};
use futures::StreamExt;
use serde::Serialize;
use tracing::{error, info, warn};

/// STEP 31b — a real, named reason a candidate copymint opportunity was
/// skipped or a manual fire attempt was refused, instead of only a
/// free-text `bus::log` line. Mirrors this file's own existing
/// `fireable`/`is_free` structured-gate pattern (step 6c): the UI gets a
/// typed code it can render a specific card/badge for, plus (via
/// `ServerEvent::CopymintSkipped::actionable_hint`) a concrete next step
/// where one genuinely exists, rather than expecting an operator to
/// parse a log line.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum CopymintSkipReason {
    /// getPublicDrop reverted, the RPC call itself failed, or this
    /// nftContract has no public SeaDrop stage configured at all.
    DropLookupFailed { detail: String },
    /// getPublicDrop succeeded but start_time/end_time show this drop
    /// isn't currently in its live window.
    NotCurrentlyLive { start_time: u64, end_time: u64, now: u64 },
    /// A paid opportunity whose total value exceeds `max_copymint_price_wei`.
    /// Still surfaces as a `CopyOpportunity` card too (fireable: false) —
    /// this is the same event, structured for a second, more actionable
    /// UI treatment, not a replacement for the existing one.
    ExceedsPriceCeiling { total_value_wei: String, ceiling_wei: String },
    /// Building the replicated mintPublic calldata failed — should not
    /// normally happen; surfaced structurally rather than only logged so
    /// it isn't silently invisible if it ever does.
    CalldataEncodingFailed { detail: String },
}

impl CopymintSkipReason {
    /// A concrete "what would fix this" string where one exists — `None`
    /// when the real fix is external state this bot has no lever over
    /// (waiting for a drop to go live isn't a config change; a genuine
    /// RPC/lookup failure could be several different underlying causes).
    pub fn actionable_hint(&self) -> Option<String> {
        match self {
            CopymintSkipReason::ExceedsPriceCeiling { total_value_wei, ceiling_wei } => Some(format!(
                "raise max_copymint_price_wei to at least {total_value_wei} (currently {ceiling_wei}) to allow this to fire"
            )),
            CopymintSkipReason::DropLookupFailed { .. } => Some(
                "check seadrop_address and RPC connectivity, or this nftContract may not have a public SeaDrop stage configured".to_string(),
            ),
            CopymintSkipReason::NotCurrentlyLive { .. } | CopymintSkipReason::CalldataEncodingFailed { .. } => None,
        }
    }
}

fn emit_skip(state: &SharedState, tracked_wallet: Address, nft_contract: Address, reason: CopymintSkipReason) {
    let actionable_hint = reason.actionable_hint();
    let _ = state.bus.send(ServerEvent::CopymintSkipped {
        tracked_wallet: format!("{tracked_wallet:#x}"),
        nft_contract: format!("{nft_contract:#x}"),
        reason,
        actionable_hint,
    });
}

/// STEP 31b — per-wallet, per-collection eligibility report.
#[derive(Debug, Clone, Serialize)]
pub struct WalletEligibility {
    pub address: String,
    pub already_minted: u64,
    pub eligible: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CopymintEligibilityReport {
    pub nft_contract: String,
    pub max_per_wallet: u16,
    pub current_total_supply: u64,
    pub max_supply: u64,
    pub remaining_supply: u64,
    pub eligible_count: u32,
    pub total_count: u32,
    pub wallets: Vec<WalletEligibility>,
}

/// Pure eligibility check for ONE wallet — no RPC, so this is the piece
/// that's actually unit-tested (see tests below). `remaining_supply` is
/// read once, shared across every wallet in the report — this is a
/// pre-fire ESTIMATE against the collection's state at the moment of the
/// check, not a guarantee: if the parallel-EOA path actually fires N
/// wallets, on-chain execution order (not this function) decides which
/// ones land before global supply is exhausted. Never used by, and has
/// no bearing on, that hot fire path itself — this exists purely to
/// inform a human before they click "check eligibility" / commit to a
/// copymint fire.
pub fn compute_wallet_eligibility(
    already_minted: u64,
    quantity_per_wallet: u64,
    max_per_wallet: u16,
    remaining_supply: u64,
) -> bool {
    let would_have = already_minted.saturating_add(quantity_per_wallet);
    would_have <= max_per_wallet as u64 && remaining_supply >= quantity_per_wallet
}

/// Checks every one of the bot's OWN configured wallets (`state.wallet_status`
/// — read-only addresses, no private key material touched) against a
/// detected copymint target's real `getPublicDrop`
/// (`maxTotalMintableByWallet`) and `getMintStats`
/// (`minterNumMinted`/`currentTotalSupply`/`maxSupply`) fields, live —
/// never cached, never guessed. Explicitly a read-only pre-fire check:
/// this function never sends `ControlMsg::FireCopymint` or touches
/// anything the parallel-EOA hot fire path depends on.
pub async fn check_eligibility(state: &SharedState, nft_contract: Address) -> Result<CopymintEligibilityReport> {
    let cfg = state.config.read().await.clone();
    let seadrop_address = resolve_seadrop_address(&cfg)?;
    let http_rpc_url = state.best_http_rpc_url().await.unwrap_or_else(|| cfg.http_rpc_urls[0].clone());

    let drop = seadrop::fetch_public_drop(&http_rpc_url, seadrop_address, nft_contract)
        .await
        .context("getPublicDrop failed")?;

    let wallet_addresses: Vec<String> = state.wallet_status.read().await.iter().map(|w| w.address.clone()).collect();
    if wallet_addresses.is_empty() {
        anyhow::bail!("no wallets configured — nothing to check eligibility for");
    }

    let mut wallets = Vec::with_capacity(wallet_addresses.len());
    let mut current_total_supply = 0u64;
    let mut max_supply = 0u64;
    for addr_str in &wallet_addresses {
        let addr: Address = addr_str.parse().context("parsing configured wallet address")?;
        let stats = seadrop::fetch_mint_stats(&http_rpc_url, nft_contract, addr)
            .await
            .with_context(|| format!("getMintStats failed for wallet {addr_str}"))?;
        current_total_supply = stats.current_total_supply;
        max_supply = stats.max_supply;
        let remaining_supply = max_supply.saturating_sub(current_total_supply);
        let eligible = compute_wallet_eligibility(stats.minter_num_minted, cfg.quantity_per_wallet, drop.max_per_wallet, remaining_supply);
        wallets.push(WalletEligibility {
            address: addr_str.clone(),
            already_minted: stats.minter_num_minted,
            eligible,
        });
    }

    let remaining_supply = max_supply.saturating_sub(current_total_supply);
    let eligible_count = wallets.iter().filter(|w| w.eligible).count() as u32;

    Ok(CopymintEligibilityReport {
        nft_contract: format!("{nft_contract:#x}"),
        max_per_wallet: drop.max_per_wallet,
        current_total_supply,
        max_supply,
        remaining_supply,
        eligible_count,
        total_count: wallets.len() as u32,
        wallets,
    })
}

/// mintPublic(address,address,address,uint256) — same signature
/// `seadrop.rs`'s `encode_mint_public` builds calldata for.
fn mint_public_fn() -> Result<Function> {
    Function::parse("mintPublic(address,address,address,uint256)")
        .context("parsing mintPublic signature")
}

/// Decodes a mintPublic call's `nftContract` (arg 0) and `feeRecipient`
/// (arg 1) out of raw calldata. Takes the FULL calldata (selector
/// included) and strips the first 4 bytes itself before decoding —
/// `abi_decode_input`'s own docs don't say precisely enough on their own
/// whether it expects the selector present or already stripped, so this
/// was verified directly against `seadrop::encode_mint_public`'s own
/// output via this file's round-trip test rather than assumed: the first
/// attempt (passing the full calldata through unstripped) decoded a
/// left-shifted, wrong address, which is exactly what you'd get from
/// decoding a tuple 4 bytes into where it actually starts — confirming
/// `abi_decode_input` expects the selector already removed.
///
/// Deliberately ignores `minterIfNotPayer` (arg 2) and `quantity` (arg
/// 3) — this module's entire safety case rests on never reading, or
/// propagating, the tracked wallet's own choice for either of those.
fn decode_mint_public_call(func: &Function, calldata: &[u8]) -> Result<(Address, Address)> {
    let args = calldata
        .get(4..)
        .context("calldata shorter than a 4-byte selector")?;
    let decoded = func
        .abi_decode_input(args)
        .context("decoding mintPublic calldata")?;
    let nft_contract = decoded
        .first()
        .and_then(|v| v.as_address())
        .context("mintPublic arg 0 (nftContract) missing or not an address")?;
    let fee_recipient = decoded
        .get(1)
        .and_then(|v| v.as_address())
        .context("mintPublic arg 1 (feeRecipient) missing or not an address")?;
    Ok((nft_contract, fee_recipient))
}

/// Whether a detected, verified opportunity should be fired
/// automatically. Deliberately does NOT take `copymint_auto_fire_paid` as
/// a parameter — see this file's doc comment. A paid opportunity
/// (`is_free = false`) returns `false` here under every possible input,
/// not just the current default; there is no argument this function
/// accepts that could make it return `true` for a paid opportunity.
fn should_auto_fire(is_free: bool, copymint_auto_fire_free: bool) -> bool {
    is_free && copymint_auto_fire_free
}

fn resolve_seadrop_address(cfg: &Config) -> Result<Address> {
    if cfg.seadrop_address.is_empty() {
        seadrop::SEADROP_1_0_MAINNET
            .parse()
            .context("hardcoded SeaDrop mainnet address failed to parse (should never happen)")
    } else {
        cfg.seadrop_address
            .parse()
            .context("bad seadrop_address in config")
    }
}

/// Runs for the process lifetime — spawn once from `main()`, same as
/// `balance_poll_loop`/`rpc_health_poll_loop`. Re-reads `tracked_wallets`
/// and the copymint config knobs fresh from `state.config` on every
/// (re)connect and every detected candidate, so a config change via
/// `PUT /api/config` takes effect without a restart — these are exactly
/// the safety-relevant knobs 6c/6e care about being live-correct.
pub async fn run_copymint_watcher(state: SharedState) {
    loop {
        let cfg = state.config.read().await.clone();

        if cfg.tracked_wallets.is_empty() {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }

        let tracked: Vec<Address> = cfg
            .tracked_wallets
            .iter()
            .filter_map(|s| match s.parse::<Address>() {
                Ok(a) => Some(a),
                Err(e) => {
                    bus::log(
                        &state.bus,
                        "warn",
                        format!("copymint: skipping unparseable tracked_wallets entry {s:?}: {e}"),
                    );
                    None
                }
            })
            .collect();

        if tracked.is_empty() {
            bus::log(
                &state.bus,
                "error",
                "copymint: tracked_wallets configured but none parsed as valid addresses — not watching",
            );
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }

        let seadrop_address = match resolve_seadrop_address(&cfg) {
            Ok(a) => a,
            Err(e) => {
                bus::log(&state.bus, "error", format!("copymint: {e:#} — not watching"));
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };

        if let Err(e) = watch_once(&state, &tracked, seadrop_address).await {
            // STEP 17 FOLLOW-UP — `bus::log` ONLY pushes onto the internal
            // event bus consumed by the UI's `/ws/events` stream (see
            // bus.rs::log — it never calls a tracing macro). That made
            // 17b's redacted-URL context wrapping on `watch_once`'s
            // `connect_ws` call invisible to `journalctl`/systemd — the
            // exact diagnostic path 17c told the operator to check —
            // even though the wrapping was correctly applied and correctly
            // reaching the UI. Also emit via `tracing::error!` so this is
            // visible from the journal alone, same as every other watcher
            // error path in this codebase.
            error!("copymint watcher error: {e:#} — reconnecting in 10s");
            bus::log(
                &state.bus,
                "error",
                format!("copymint watcher error: {e:#} — reconnecting in 10s"),
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

async fn watch_once(state: &SharedState, tracked: &[Address], seadrop_address: Address) -> Result<()> {
    let cfg = state.config.read().await.clone();

    let ws = WsConnect::new(cfg.ws_rpc_url.clone());
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_ws(ws)
        .await
        .with_context(|| {
            format!(
                "copymint: opening the WebSocket RPC connection to {}",
                crate::config::redact_rpc_url(&cfg.ws_rpc_url)
            )
        })?;

    let sub = provider.subscribe_full_pending_transactions().await.context(
        "copymint: subscribing to full pending transactions — needs a WebSocket RPC on a node \
         that exposes full mempool visibility (Geth 1.11+); many public/free RPC endpoints don't \
         support this even over WS — same requirement as trigger_mode = \"mempool_watch\", see \
         watcher.rs::run_mempool_watcher's doc comment",
    )?;

    info!(
        tracked_count = tracked.len(),
        %seadrop_address,
        "copymint: subscribed to full pending transactions"
    );
    bus::log(
        &state.bus,
        "info",
        format!("copymint watcher active — watching {} tracked wallet(s)", tracked.len()),
    );

    let func = mint_public_fn()?;
    let selector = func.selector();

    let mut stream = sub.into_stream();
    while let Some(tx) = stream.next().await {
        let sender = tx.inner.signer();
        if !tracked.contains(&sender) {
            continue;
        }
        if tx.to() != Some(seadrop_address) {
            continue;
        }
        let input = tx.input();
        if input.len() < 4 || input[..4] != selector[..] {
            continue;
        }

        match decode_mint_public_call(&func, input) {
            Ok((nft_contract, fee_recipient)) => {
                handle_candidate(state, sender, nft_contract, fee_recipient).await;
            }
            Err(e) => {
                warn!(error = %e, %sender, "copymint: matched mintPublic selector but failed to decode args");
            }
        }
    }
    Ok(())
}

/// A tracked wallet's pending mintPublic call matched — now do the thing
/// that stands in for the human review every other trigger mode gets
/// from being manually configured: independently confirm, via a fresh
/// `getPublicDrop` read (never trusting anything from the tracked
/// wallet's own calldata beyond "this is the nftContract/feeRecipient
/// they used"), that this is a real, currently-live drop before treating
/// it as an opportunity at all.
async fn handle_candidate(
    state: &SharedState,
    tracked_wallet: Address,
    nft_contract: Address,
    fee_recipient: Address,
) {
    let cfg = state.config.read().await.clone();
    let seadrop_address = match resolve_seadrop_address(&cfg) {
        Ok(a) => a,
        Err(e) => {
            bus::log(&state.bus, "error", format!("copymint: {e:#}"));
            return;
        }
    };

    // STEP 29c — prefer the fastest-known-healthy configured RPC for this
    // verification read, same as target resolution's own use of
    // AppState::best_http_rpc_url(). Falls back to cfg.http_rpc_urls[0]
    // exactly as before if ranking data isn't available yet.
    let http_rpc_url = state.best_http_rpc_url().await.unwrap_or_else(|| cfg.http_rpc_urls[0].clone());
    let drop = match seadrop::fetch_public_drop(&http_rpc_url, seadrop_address, nft_contract).await {
        Ok(d) => d,
        Err(e) => {
            bus::log(
                &state.bus,
                "warn",
                format!(
                    "copymint: tracked wallet {tracked_wallet:#x} minted {nft_contract:#x} but \
                     getPublicDrop failed ({e:#}) — not treating as an opportunity"
                ),
            );
            emit_skip(state, tracked_wallet, nft_contract, CopymintSkipReason::DropLookupFailed { detail: format!("{e:#}") });
            return;
        }
    };

    let now = bus::now_ts();
    if !(drop.start_time <= now && now <= drop.end_time) {
        bus::log(
            &state.bus,
            "warn",
            format!(
                "copymint: tracked wallet {tracked_wallet:#x} minted {nft_contract:#x} but \
                 getPublicDrop shows it's not currently live (start {}, end {}, now {}) — not \
                 treating as an opportunity",
                drop.start_time, drop.end_time, now
            ),
        );
        emit_skip(
            state,
            tracked_wallet,
            nft_contract,
            CopymintSkipReason::NotCurrentlyLive { start_time: drop.start_time, end_time: drop.end_time, now },
        );
        return;
    }

    let is_free = drop.mint_price_wei == U256::ZERO;
    let total_value = drop.mint_price_wei * U256::from(cfg.quantity_per_wallet);
    // Ceiling applies to paid opportunities specifically — free is exempt
    // by definition, per 6c ("free is 0 by definition and always passes
    // it"), not because the ceiling happens to be large enough.
    let fireable = is_free || total_value <= U256::from(cfg.max_copymint_price_wei);

    let _ = state.bus.send(ServerEvent::CopyOpportunity {
        tracked_wallet: format!("{tracked_wallet:#x}"),
        nft_contract: format!("{nft_contract:#x}"),
        fee_recipient: format!("{fee_recipient:#x}"),
        mint_price_wei: drop.mint_price_wei.to_string(),
        total_value_wei: total_value.to_string(),
        quantity: cfg.quantity_per_wallet,
        is_free,
        fireable,
    });
    bus::log(
        &state.bus,
        "warn",
        format!(
            "copymint opportunity: tracked wallet {tracked_wallet:#x} minting {nft_contract:#x} — {}",
            if is_free {
                "FREE (auto-fire eligible)".to_string()
            } else if fireable {
                format!("{total_value} wei — requires manual fire")
            } else {
                format!("{total_value} wei — EXCEEDS max_copymint_price_wei, not fireable")
            }
        ),
    );
    if !is_free && !fireable {
        emit_skip(
            state,
            tracked_wallet,
            nft_contract,
            CopymintSkipReason::ExceedsPriceCeiling {
                total_value_wei: total_value.to_string(),
                ceiling_wei: cfg.max_copymint_price_wei.to_string(),
            },
        );
    }

    if should_auto_fire(is_free, cfg.copymint_auto_fire_free) {
        match seadrop::encode_mint_public(nft_contract, fee_recipient, cfg.quantity_per_wallet) {
            Ok(calldata) => {
                let _ = state
                    .control_tx
                    .send(ControlMsg::FireCopymint {
                        contract: seadrop_address,
                        calldata,
                        value: total_value,
                    })
                    .await;
            }
            Err(e) => {
                bus::log(
                    &state.bus,
                    "error",
                    format!("copymint: failed to encode mintPublic calldata for {nft_contract:#x}: {e:#}"),
                );
                emit_skip(state, tracked_wallet, nft_contract, CopymintSkipReason::CalldataEncodingFailed { detail: format!("{e:#}") });
            }
        }
    }
}

/// Fetches a fresh `getPublicDrop` read and enqueues a `FireCopymint` if,
/// and only if, it's still live and within `max_copymint_price_wei` —
/// used by `api.rs`'s manual-fire route so a stale/cached client-side
/// price is never what a real fire decision is based on. Returns the
/// verified total value (wei) on success, or an error describing why it
/// refused to fire.
pub async fn verify_and_fire(
    state: &SharedState,
    nft_contract: Address,
    fee_recipient: Address,
) -> Result<U256> {
    let cfg = state.config.read().await.clone();
    let seadrop_address = resolve_seadrop_address(&cfg)?;

    // STEP 29c — see handle_candidate's identical comment above.
    let http_rpc_url = state.best_http_rpc_url().await.unwrap_or_else(|| cfg.http_rpc_urls[0].clone());
    let drop = seadrop::fetch_public_drop(&http_rpc_url, seadrop_address, nft_contract)
        .await
        .context("getPublicDrop failed")?;

    let now = bus::now_ts();
    if !(drop.start_time <= now && now <= drop.end_time) {
        anyhow::bail!("drop is not currently live (start {}, end {}, now {})", drop.start_time, drop.end_time, now);
    }

    let is_free = drop.mint_price_wei == U256::ZERO;
    let total_value = drop.mint_price_wei * U256::from(cfg.quantity_per_wallet);
    if !is_free && total_value > U256::from(cfg.max_copymint_price_wei) {
        anyhow::bail!(
            "total value {total_value} wei exceeds max_copymint_price_wei ({} wei) — refusing to fire",
            cfg.max_copymint_price_wei
        );
    }

    let calldata = seadrop::encode_mint_public(nft_contract, fee_recipient, cfg.quantity_per_wallet)
        .context("encoding mintPublic calldata")?;

    bus::log(
        &state.bus,
        "warn",
        format!("manual copymint FIRE requested via UI for {nft_contract:#x} ({total_value} wei)"),
    );
    let _ = state
        .control_tx
        .send(ControlMsg::FireCopymint {
            contract: seadrop_address,
            calldata,
            value: total_value,
        })
        .await;

    Ok(total_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::dyn_abi::DynSolValue;

    #[test]
    fn decode_mint_public_call_round_trips_against_our_own_encoder() {
        let nft_contract: Address = "0x603a481580c8Cf85ee169b315653bd9D33C39e52".parse().unwrap();
        let fee_recipient: Address = "0x0000a26b00c1F0DF003000390027140000fAa719".parse().unwrap();
        let calldata = seadrop::encode_mint_public(nft_contract, fee_recipient, 3).unwrap();

        let func = mint_public_fn().unwrap();
        let (decoded_nft, decoded_fee) = decode_mint_public_call(&func, &calldata).unwrap();
        assert_eq!(decoded_nft, nft_contract);
        assert_eq!(decoded_fee, fee_recipient);
    }

    /// 6e, safety property #1: does our replicated mintPublic call mint
    /// to OUR wallet under all circumstances, never the tracked wallet?
    /// Simulates the adversarial case 6a exists to prevent — a tracked
    /// wallet's own pending calldata sets minterIfNotPayer to THEIR OWN
    /// address, hand-built (NOT via seadrop::encode_mint_public, which
    /// always uses ZERO — specifically to prove this path doesn't depend
    /// on that being true of the input).
    #[test]
    fn copymint_never_propagates_tracked_wallets_own_minter_choice() {
        let tracked_wallet_own_address: Address =
            "0x00000000000000000000000000000000DeaDBeef".parse().unwrap();
        let nft_contract: Address = "0x603a481580c8Cf85ee169b315653bd9D33C39e52".parse().unwrap();
        let fee_recipient: Address = "0x0000a26b00c1F0DF003000390027140000fAa719".parse().unwrap();

        let func = mint_public_fn().unwrap();
        let their_calldata = func
            .abi_encode_input(&[
                DynSolValue::Address(nft_contract),
                DynSolValue::Address(fee_recipient),
                DynSolValue::Address(tracked_wallet_own_address), // THEIR minterIfNotPayer
                DynSolValue::Uint(U256::from(1u64), 256),
            ])
            .unwrap();

        let (decoded_nft, decoded_fee) = decode_mint_public_call(&func, &their_calldata).unwrap();
        assert_eq!(decoded_nft, nft_contract);
        assert_eq!(decoded_fee, fee_recipient);

        // Build OUR OWN call from the decoded values, exactly as
        // handle_candidate does, and confirm minterIfNotPayer is ZERO in
        // OUR calldata — not the tracked wallet's address, regardless of
        // what their own calldata said.
        let our_calldata = seadrop::encode_mint_public(decoded_nft, decoded_fee, 5).unwrap();
        let our_decoded = func.abi_decode_input(&our_calldata).unwrap();
        let our_minter = our_decoded[2].as_address().unwrap();
        assert_eq!(
            our_minter,
            Address::ZERO,
            "minterIfNotPayer must always be ZERO (mint-to-payer) in our own call"
        );
        assert_ne!(our_minter, tracked_wallet_own_address);
    }

    /// 6e, safety property #2: a paid opportunity requires a manual click
    /// under the default config, with no code path that could auto-fire
    /// it. `should_auto_fire` doesn't even accept `copymint_auto_fire_paid`
    /// as an argument — this asserts the property that signature implies:
    /// false for every possible value of the free-auto-fire toggle too,
    /// since a paid opportunity must never auto-fire under ANY config.
    #[test]
    fn paid_opportunities_never_auto_fire_regardless_of_config() {
        assert!(!should_auto_fire(false, true));
        assert!(!should_auto_fire(false, false));
    }

    #[test]
    fn free_opportunities_auto_fire_only_when_enabled() {
        assert!(should_auto_fire(true, true));
        assert!(!should_auto_fire(true, false));
    }

    // --- STEP 31b: eligibility check + structured skip reasons ---

    #[test]
    fn wallet_under_cap_with_supply_remaining_is_eligible() {
        assert!(compute_wallet_eligibility(3, 1, 20, 500));
    }

    #[test]
    fn wallet_at_cap_is_not_eligible() {
        // already_minted (20) + quantity_per_wallet (1) > max_per_wallet (20)
        assert!(!compute_wallet_eligibility(20, 1, 20, 500));
    }

    #[test]
    fn wallet_exactly_at_the_boundary_is_eligible() {
        // already_minted (19) + quantity_per_wallet (1) == max_per_wallet (20)
        assert!(compute_wallet_eligibility(19, 1, 20, 500));
    }

    #[test]
    fn wallet_under_cap_but_no_supply_remaining_is_not_eligible() {
        // This is the real EVERYBODYS state confirmed live in step 31b's
        // research: currentTotalSupply == maxSupply == 10000, so
        // remaining_supply is 0 regardless of any wallet's own cap room.
        assert!(!compute_wallet_eligibility(0, 1, 20, 0));
    }

    #[test]
    fn requesting_more_than_one_quantity_is_checked_against_both_gates() {
        // Room under the per-wallet cap (18+5=23 > 20 cap) fails even
        // though supply alone would allow it.
        assert!(!compute_wallet_eligibility(18, 5, 20, 500));
        // Enough cap room but not enough remaining supply for the full
        // requested quantity.
        assert!(!compute_wallet_eligibility(0, 5, 20, 3));
    }

    #[test]
    fn already_minted_does_not_overflow_on_saturating_add() {
        // A pathological already_minted (shouldn't happen from a real
        // getMintStats read, but the function must not panic) never
        // wraps back around to "eligible".
        assert!(!compute_wallet_eligibility(u64::MAX, 1, 20, 500));
    }

    #[test]
    fn exceeds_price_ceiling_has_an_actionable_hint() {
        let reason = CopymintSkipReason::ExceedsPriceCeiling {
            total_value_wei: "5000000000000000".to_string(),
            ceiling_wei: "1000000000000000".to_string(),
        };
        let hint = reason.actionable_hint().expect("should have a concrete next step");
        assert!(hint.contains("max_copymint_price_wei"));
        assert!(hint.contains("5000000000000000"));
    }

    #[test]
    fn not_currently_live_has_no_actionable_hint() {
        // Waiting for a drop to go live (or accepting it already ended)
        // isn't a config change this bot can make on the operator's
        // behalf — no hint should be fabricated for it.
        let reason = CopymintSkipReason::NotCurrentlyLive { start_time: 100, end_time: 200, now: 50 };
        assert!(reason.actionable_hint().is_none());
    }

    #[test]
    fn calldata_encoding_failed_has_no_actionable_hint() {
        let reason = CopymintSkipReason::CalldataEncodingFailed { detail: "bad abi".to_string() };
        assert!(reason.actionable_hint().is_none());
    }

    #[test]
    fn drop_lookup_failed_has_an_actionable_hint() {
        let reason = CopymintSkipReason::DropLookupFailed { detail: "rpc timeout".to_string() };
        assert!(reason.actionable_hint().is_some());
    }
}
