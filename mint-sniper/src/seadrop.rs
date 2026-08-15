//! SeaDrop 1.0 integration.
//!
//! SeaDrop is OpenSea's open-source, audited, singleton minting contract
//! (Spearbit-audited, MIT licensed). Public mints go through ONE fixed
//! contract address regardless of which project's drop you're targeting —
//! that's what makes this a first-class mode here instead of the generic
//! "guess the project's mint() signature" path in executor.rs/config.rs.
//!
//! Deployment (same address on every chain it's deployed to, deterministic
//! CREATE2 deploy): 0x00005EA00Ac477B1030CE78506496e8C2dE24bf5
//! Confirmed on Ethereum mainnet and Polygon as of this writing —
//! verify on the target chain's block explorer before relying on it,
//! deployments do vary by chain and this list can go stale.
//!
//! IMPORTANT SCOPE LIMIT: this only covers `mintPublic` — the no-allowlist,
//! no-signature public stage. SeaDrop also supports `mintAllowList` (needs
//! a merkle proof), `mintSigned` (needs a server-issued EIP-712 signature
//! from the project), and `mintAllowedTokenHolder` (needs a gating token
//! already in the wallet). None of those are scriptable from just an RPC
//! connection — the proof/signature/token has to come from somewhere, and
//! that "somewhere" is project-specific out-of-band data this bot doesn't
//! have. If a drop you're targeting isn't in its public stage, this module
//! doesn't help you snipe it.

use alloy::dyn_abi::DynSolValue;
use alloy::json_abi::Function;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use anyhow::{Context, Result};

pub const SEADROP_1_0_MAINNET: &str = "0x00005EA00Ac477B1030CE78506496e8C2dE24bf5";

#[derive(Debug, Clone, Copy)]
pub struct PublicDropInfo {
    pub mint_price_wei: U256,
    pub start_time: u64,
    pub end_time: u64,
    pub max_per_wallet: u16,
    pub restrict_fee_recipients: bool,
}

/// Reads ISeaDrop.getPublicDrop(nftContract) from the singleton contract.
/// Struct layout per SeaDropStructs.sol (packed into one storage slot
/// on-chain, but ABI-decodes as six ordinary right-padded words):
///   mintPrice: uint80, startTime: uint48, endTime: uint48,
///   maxTotalMintableByWallet: uint16, feeBps: uint16,
///   restrictFeeRecipients: bool
pub async fn fetch_public_drop(
    http_rpc: &str,
    seadrop: Address,
    nft_contract: Address,
) -> Result<PublicDropInfo> {
    let provider = ProviderBuilder::new().on_http(http_rpc.parse()?);

    let func = Function::parse(
        "getPublicDrop(address) returns (uint80,uint48,uint48,uint16,uint16,bool)",
    )
    .context("parsing getPublicDrop signature")?;

    let calldata = func.abi_encode_input(&[DynSolValue::Address(nft_contract)])?;
    let tx = TransactionRequest::default().to(seadrop).input(calldata.into());

    let raw = provider
        .call(&tx)
        .await
        .context("getPublicDrop call failed — check seadrop address and chain")?;

    let decoded = func
        .abi_decode_output(&raw, true)
        .context("decoding PublicDrop struct")?;

    // Field order matches the struct declaration above, not storage-slot
    // packing order — the ABI decoder handles unpacking.
    let mint_price_wei = decoded[0]
        .as_uint()
        .map(|(v, _)| v)
        .context("mintPrice field")?;
    let start_time = decoded[1]
        .as_uint()
        .map(|(v, _)| v.to::<u64>())
        .context("startTime field")?;
    let end_time = decoded[2]
        .as_uint()
        .map(|(v, _)| v.to::<u64>())
        .context("endTime field")?;
    let max_per_wallet = decoded[3]
        .as_uint()
        .map(|(v, _)| v.to::<u64>() as u16)
        .context("maxTotalMintableByWallet field")?;
    let restrict_fee_recipients = decoded[5].as_bool().context("restrictFeeRecipients field")?;

    if start_time == 0 {
        anyhow::bail!(
            "getPublicDrop returned startTime=0 — this nftContract likely has no \
             public drop stage configured (it may be allowlist/signed-mint only), \
             or the address is wrong"
        );
    }

    Ok(PublicDropInfo {
        mint_price_wei,
        start_time,
        end_time,
        max_per_wallet,
        restrict_fee_recipients,
    })
}

/// Checks getFeeRecipientIsAllowed(nftContract, feeRecipient) — call this
/// before firing if `restrict_fee_recipients` came back true. A fee
/// recipient that isn't on the project's allowlist reverts every mint tx
/// with FeeRecipientNotAllowed, which you want to know at arm-time, not
/// after burning gas on N reverted wallets.
pub async fn is_fee_recipient_allowed(
    http_rpc: &str,
    seadrop: Address,
    nft_contract: Address,
    fee_recipient: Address,
) -> Result<bool> {
    let provider = ProviderBuilder::new().on_http(http_rpc.parse()?);
    let func = Function::parse("getFeeRecipientIsAllowed(address,address) returns (bool)")
        .context("parsing getFeeRecipientIsAllowed signature")?;
    let calldata = func.abi_encode_input(&[
        DynSolValue::Address(nft_contract),
        DynSolValue::Address(fee_recipient),
    ])?;
    let tx = TransactionRequest::default().to(seadrop).input(calldata.into());
    let raw = provider.call(&tx).await.context("getFeeRecipientIsAllowed call failed")?;
    let decoded = func.abi_decode_output(&raw, true)?;
    decoded[0].as_bool().context("decoding bool result")
}

/// Builds calldata for ISeaDrop.mintPublic(nftContract, feeRecipient,
/// minterIfNotPayer, quantity). minterIfNotPayer = Address::ZERO means
/// "mint to msg.sender" — each wallet mints to itself, the normal case for
/// a sniper (as opposed to routing every mint to one collector wallet,
/// which is possible but is a deliberate choice, not the default).
pub fn encode_mint_public(
    nft_contract: Address,
    fee_recipient: Address,
    quantity: u64,
) -> Result<Vec<u8>> {
    let func = Function::parse("mintPublic(address,address,address,uint256)")
        .context("parsing mintPublic signature")?;
    let values = vec![
        DynSolValue::Address(nft_contract),
        DynSolValue::Address(fee_recipient),
        DynSolValue::Address(Address::ZERO),
        DynSolValue::Uint(U256::from(quantity), 256),
    ];
    Ok(func.abi_encode_input(&values)?)
}
