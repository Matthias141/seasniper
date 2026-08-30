//! Target resolution (step 8b): turns a pasted address or OpenSea
//! collection URL into a verified SeaDrop target — price/timing/
//! maxPerWallet read fresh from `getPublicDrop`, plus whether the
//! currently-configured `fee_recipient` is actually usable on this
//! specific drop. Resolving never changes bot state on its own —
//! `api.rs`'s `/api/target/resolve` route is read-only; only
//! `/api/target/set` (a separate, explicit action) commits anything, and
//! it re-runs this same verification fresh rather than trusting a
//! client-echoed result from an earlier `/resolve` call.

use crate::config::Config;
use crate::goplus;
use crate::opensea;
use crate::seadrop;
use crate::state::SharedState;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{Context, Result};

pub struct ResolvedTarget {
    pub nft_contract: Address,
    pub name: Option<String>,
    pub links: opensea::OfficialLinks,
    pub mint_price_wei: U256,
    pub start_time: u64,
    pub end_time: u64,
    pub max_per_wallet: u16,
    pub restrict_fee_recipients: bool,
    pub fee_recipient: Address,
    /// Whether the currently-configured `fee_recipient` would actually
    /// be accepted on this specific drop. Always `true` when
    /// `restrict_fee_recipients` is `false` (any recipient is accepted).
    /// A resolution with this `false` should not be offered as
    /// one-click-settable in the UI — see `api.rs`'s `/api/target/set`
    /// handler, which refuses to proceed if this is `false`.
    pub fee_recipient_ok: bool,
    pub quantity_per_wallet: u64,

    /// --- step 29b: additional pre-arm signals, alongside the existing
    /// namesquatting warning (8c) — neither of these blocks resolution or
    /// replaces human judgment; both fail open (`None`) on any check
    /// failure rather than ever silently treating a failed check as
    /// "confirmed clean." ---
    /// `None` = could not be determined (RPC error, or the contract has
    /// no code at the address at all — which `getPublicDrop` succeeding
    /// above already makes unlikely). A very small value is the real
    /// namesquatting signal this exists to catch: a fake collection
    /// deployed minutes or hours before someone searches for the real
    /// drop's name.
    pub contract_age_secs: Option<u64>,
    pub goplus: goplus::NftSecurityCheck,
}

/// Resolves free-text `input` (a raw address or an OpenSea collection
/// URL — see `opensea::parse_input`) to a verified target. `input` that's
/// neither is rejected with a clear error — this function does not fall
/// back to treating it as a search query; that's 8c's separate,
/// explicitly-picklist-based flow, not something to guess into here.
pub async fn resolve(state: &SharedState, http_client: &reqwest::Client, input: &str) -> Result<ResolvedTarget> {
    let cfg = state.config.read().await.clone();

    match opensea::parse_input(input) {
        Some(opensea::ResolvedInput::Address(nft_contract)) => resolve_address(&cfg, nft_contract).await,
        Some(opensea::ResolvedInput::OpenSeaSlug(slug)) => {
            let api_key = cfg.resolve_opensea_api_key();
            let collection = opensea::resolve_slug(http_client, api_key.as_deref(), &slug).await?;
            let mut resolved = resolve_address(&cfg, collection.nft_contract).await?;
            resolved.name = Some(collection.name);
            resolved.links = collection.links;
            Ok(resolved)
        }
        None => anyhow::bail!(
            "input isn't a recognizable contract address or OpenSea collection URL — for a name \
             search, use the search endpoint instead (step 8c)"
        ),
    }
}

/// Re-verifies an already-known `nft_contract` fresh — used both by
/// `resolve`'s address branch and by `/api/target/set`, which never
/// trusts a client-supplied resolution and always re-derives everything
/// from a live `getPublicDrop` call immediately before committing.
pub async fn resolve_address(cfg: &Config, nft_contract: Address) -> Result<ResolvedTarget> {
    let seadrop_address: Address = if cfg.seadrop_address.is_empty() {
        seadrop::SEADROP_1_0_MAINNET
            .parse()
            .context("hardcoded SeaDrop mainnet address failed to parse (should never happen)")?
    } else {
        cfg.seadrop_address.parse().context("bad seadrop_address in config")?
    };

    let drop = seadrop::fetch_public_drop(&cfg.http_rpc_urls[0], seadrop_address, nft_contract)
        .await
        .context("getPublicDrop failed")?;

    let fee_recipient: Address = cfg
        .fee_recipient
        .parse()
        .context("current fee_recipient in config is not a valid address — set one before resolving a target")?;

    let fee_recipient_ok = if drop.restrict_fee_recipients {
        seadrop::is_fee_recipient_allowed(&cfg.http_rpc_urls[0], seadrop_address, nft_contract, fee_recipient)
            .await
            .unwrap_or(false)
    } else {
        true
    };

    // STEP 29b — best-effort, never blocks resolution on failure (both
    // fail open independently of each other and of the getPublicDrop
    // result above, which is the one check that DOES gate resolution).
    let contract_age_secs = estimate_contract_age_secs(&cfg.http_rpc_urls[0], nft_contract).await;
    let goplus = match http_client_and_chain_id(&cfg.http_rpc_urls[0]).await {
        Some((client, chain_id)) => goplus::check(&client, chain_id, nft_contract).await,
        None => goplus::NftSecurityCheck::default(),
    };

    Ok(ResolvedTarget {
        nft_contract,
        name: None,
        links: opensea::OfficialLinks::default(),
        mint_price_wei: drop.mint_price_wei,
        start_time: drop.start_time,
        end_time: drop.end_time,
        max_per_wallet: drop.max_per_wallet,
        restrict_fee_recipients: drop.restrict_fee_recipients,
        fee_recipient,
        fee_recipient_ok,
        quantity_per_wallet: cfg.quantity_per_wallet,
        contract_age_secs,
        goplus,
    })
}

/// STEP 29b — a fresh, short-lived `reqwest::Client` plus the chain's own
/// live `chain_id` (never assumed/cached, same "read live" principle
/// step 13b/22c already established for this codebase's other chain-id
/// reads). `None` on any RPC failure — the GoPlus check this feeds is
/// already fail-open on its own, so a failure here just means "skip the
/// GoPlus check for this resolution," not a resolution failure.
async fn http_client_and_chain_id(http_rpc_url: &str) -> Option<(reqwest::Client, u64)> {
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(http_rpc_url.parse().ok()?);
    let chain_id = provider.get_chain_id().await.ok()?;
    Some((reqwest::Client::new(), chain_id))
}

/// STEP 29b — estimates `address`'s deployment block via binary search on
/// `eth_getCode` (present vs. absent at a given height), then reads that
/// block's real timestamp — no external indexer or Etherscan-style API
/// needed, so this works identically on any EVM chain this codebase
/// targets, including ones (like Robinhood Chain testnet, per step 22a)
/// no third-party contract-age service covers at all. Deliberately NOT
/// delegated to GoPlus's own `create_block_number` field: that field is
/// null on plenty of real contracts (confirmed live — GoPlus hadn't
/// indexed it for at least one real, checked collection this session),
/// and a namesquatting fake is exactly the kind of low-profile, very
/// recently deployed contract a security scanner is LEAST likely to have
/// indexed yet — relying on GoPlus alone would systematically miss the
/// adversarial case this check exists to catch.
///
/// Bounded to `ceil(log2(latest_block))` RPC round trips (~26 on a chain
/// with tens of millions of blocks, fewer on a young chain like Robinhood
/// or InkChain) — not free, but this only runs on an explicit
/// resolve/set action (an operator pointing the bot at a candidate
/// target), never on the hot prepare/fire path. Returns `None` on any
/// RPC error (never guesses a wrong age from a partial search) or if the
/// contract has no code at the current block at all.
async fn estimate_contract_age_secs(http_rpc_url: &str, address: Address) -> Option<u64> {
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(http_rpc_url.parse().ok()?);

    let latest = provider.get_block_number().await.ok()?;
    if provider.get_code_at(address).number(latest).await.ok()?.is_empty() {
        return None; // no code at all — not our job to explain why here
    }

    let mut lo = 0u64;
    let mut hi = latest;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let has_code = !provider.get_code_at(address).number(mid).await.ok()?.is_empty();
        if has_code {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let deployment_block = lo;

    let block = provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(deployment_block))
        .await
        .ok()??;
    let deployed_at = block.header.inner.timestamp;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(deployed_at))
}

impl ResolvedTarget {
    /// Whether `now` falls within the drop's live window. Independent of
    /// `fee_recipient_ok` — a caller deciding whether to offer "set as
    /// active target" should check both.
    pub fn is_live(&self, now: u64) -> bool {
        self.start_time <= now && now <= self.end_time
    }

    pub fn total_value_wei(&self) -> U256 {
        self.mint_price_wei * U256::from(self.quantity_per_wallet)
    }

    pub fn mint_calldata(&self) -> Result<Vec<u8>> {
        seadrop::encode_mint_public(self.nft_contract, self.fee_recipient, self.quantity_per_wallet)
    }
}
