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
//! Confirmed on Ethereum mainnet and Polygon as of this writing, and (step
//! 13a) Robinhood Chain mainnet (4663) AND testnet (46630) — real
//! `eth_getCode` calls against both, not assumed from
//! morsyxbt/nft-public-mint's chain-support list alone. Runtime bytecode
//! is byte-identical to Ethereum mainnet's own deployment except for one
//! ~20-byte segment containing the literal chain id (an EIP-712 domain
//! separator immutable, baked in per-chain at deploy time — CREATE2 keeps
//! the ADDRESS deterministic from init-code+salt alone, but the compiled
//! runtime code legitimately differs by this one chain-dependent constant;
//! this is expected and not a sign of a different/tampered deployment).
//! `SEADROP_1_0_MAINNET`'s name is a holdover from when only Ethereum
//! mainnet was in scope — despite the name, this same constant is the
//! correct default on every chain confirmed above, not a mainnet-only
//! value. Still verify on any NEW target chain's block explorer before
//! relying on it — deployments do vary by chain and this list can go
//! stale.
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

use alloy::dyn_abi::{DynSolValue, FunctionExt, JsonAbiExt};
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
    let provider = ProviderBuilder::new().disable_recommended_fillers().connect_http(http_rpc.parse()?);

    let func = Function::parse(
        "getPublicDrop(address) returns (uint80,uint48,uint48,uint16,uint16,bool)",
    )
    .context("parsing getPublicDrop signature")?;

    let calldata = func.abi_encode_input(&[DynSolValue::Address(nft_contract)])?;
    let tx = TransactionRequest::default().to(seadrop).input(calldata.into());

    let raw = provider
        .call(tx)
        .await
        .context("getPublicDrop call failed — check seadrop address and chain")?;

    let decoded = func
        .abi_decode_output(&raw)
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
    let provider = ProviderBuilder::new().disable_recommended_fillers().connect_http(http_rpc.parse()?);
    let func = Function::parse("getFeeRecipientIsAllowed(address,address) returns (bool)")
        .context("parsing getFeeRecipientIsAllowed signature")?;
    let calldata = func.abi_encode_input(&[
        DynSolValue::Address(nft_contract),
        DynSolValue::Address(fee_recipient),
    ])?;
    let tx = TransactionRequest::default().to(seadrop).input(calldata.into());
    let raw = provider.call(tx).await.context("getFeeRecipientIsAllowed call failed")?;
    let decoded = func.abi_decode_output(&raw)?;
    decoded[0].as_bool().context("decoding bool result")
}

#[derive(Debug, Clone, Copy)]
pub struct MintStats {
    pub minter_num_minted: u64,
    pub current_total_supply: u64,
    pub max_supply: u64,
}

/// Reads IERC721SeaDrop.getMintStats(minter) from the NFT contract itself
/// (NOT the SeaDrop singleton — this is a per-collection function every
/// ERC721SeaDrop-family contract implements). Confirmed live, not
/// assumed: `eth_call` against a real mainnet SeaDrop collection
/// (EVERYBODYS, the same one seadrop.rs's own encode_mint_public test
/// uses) with selector 0x840e15d4 (keccak256("getMintStats(address)")[..4],
/// independently computed) returned a real, well-formed 3-word response —
/// decoded here as (minterNumMinted, currentTotalSupply, maxSupply), per
/// step 23's own design note on this function. Used by copymint's
/// pre-fire eligibility check (31b) — never on the parallel-EOA hot fire
/// path, which has no eligibility-check concept and isn't touched by
/// this function existing.
pub async fn fetch_mint_stats(http_rpc: &str, nft_contract: Address, minter: Address) -> Result<MintStats> {
    let provider = ProviderBuilder::new().disable_recommended_fillers().connect_http(http_rpc.parse()?);
    let func = Function::parse("getMintStats(address) returns (uint256,uint256,uint256)")
        .context("parsing getMintStats signature")?;
    let calldata = func.abi_encode_input(&[DynSolValue::Address(minter)])?;
    let tx = TransactionRequest::default().to(nft_contract).input(calldata.into());
    let raw = provider
        .call(tx)
        .await
        .context("getMintStats call failed — nft_contract may not implement IERC721SeaDrop")?;
    let decoded = func.abi_decode_output(&raw).context("decoding getMintStats result")?;
    let minter_num_minted = decoded[0].as_uint().map(|(v, _)| v.to::<u64>()).context("minterNumMinted field")?;
    let current_total_supply = decoded[1].as_uint().map(|(v, _)| v.to::<u64>()).context("currentTotalSupply field")?;
    let max_supply = decoded[2].as_uint().map(|(v, _)| v.to::<u64>()).context("maxSupply field")?;
    Ok(MintStats {
        minter_num_minted,
        current_total_supply,
        max_supply,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_mint_public_matches_known_good_calldata() {
        // Real mainnet SeaDrop collection — EVERYBODYS, verified against
        // getPublicDrop on-chain in step 2's audit (see git history for
        // that verification's full trace against an independent manual
        // ABI decode). Fee recipient is OpenSea's documented common
        // default, also from config.example.toml.
        let nft_contract: Address = "0x603a481580c8Cf85ee169b315653bd9D33C39e52".parse().unwrap();
        let fee_recipient: Address = "0x0000a26b00c1F0DF003000390027140000fAa719".parse().unwrap();
        let quantity = 3u64;

        let calldata = encode_mint_public(nft_contract, fee_recipient, quantity).unwrap();

        // Built independently of encode_mint_public's own logic — plain
        // byte concatenation against the ABI spec, not a round-trip
        // through the same encoder being tested.
        //
        // Selector: keccak256("mintPublic(address,address,address,uint256)")[..4]
        // = 0x161ac21f, computed and cross-checked independently via
        // alloy::primitives::keccak256 in a standalone scratch binary
        // (see step 4c's report) rather than trusted from memory.
        let mut expected: Vec<u8> = vec![0x16, 0x1a, 0xc2, 0x1f];
        expected.extend_from_slice(nft_contract.into_word().as_slice()); // arg0: nftContract
        expected.extend_from_slice(fee_recipient.into_word().as_slice()); // arg1: feeRecipient
        expected.extend_from_slice(&[0u8; 32]); // arg2: minterIfNotPayer = Address::ZERO
        expected.extend_from_slice(&U256::from(quantity).to_be_bytes::<32>()); // arg3: quantity

        assert_eq!(calldata, expected);
        assert_eq!(calldata.len(), 4 + 4 * 32); // selector + 4 words, no more no less
    }
}
