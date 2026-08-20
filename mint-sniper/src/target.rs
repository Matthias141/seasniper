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
use crate::opensea;
use crate::seadrop;
use crate::state::SharedState;
use alloy::primitives::{Address, U256};
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
    })
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
