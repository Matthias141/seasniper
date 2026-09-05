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
use std::future::IntoFuture;

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
///
/// STEP 35 — the read is bounded by `SEADROP_READ_TIMEOUT` (see
/// `fetch_public_drop_with_timeout`). Signature unchanged; every caller
/// sees the same `Result` it already handles.
pub async fn fetch_public_drop(
    http_rpc: &str,
    seadrop: Address,
    nft_contract: Address,
) -> Result<PublicDropInfo> {
    fetch_public_drop_with_timeout(http_rpc, seadrop, nft_contract, SEADROP_READ_TIMEOUT).await
}

/// STEP 35 — generous-but-bounded ceiling for one `getPublicDrop` read,
/// matching steps 33/34's 10s convention. Every measured read latency in
/// this project is sub-second (n=100 p99 was 414ms — see CLAUDE.md's step
/// 28 (final close, corrected)), so 10s is far above a slow-but-real
/// network path and far below "hang forever". This function runs on
/// main.rs's boot-time fetch (a stalled read there now fails boot loudly,
/// see `fetch_public_drop_with_timeout`'s doc below) and on main.rs's
/// Prepare-time price re-check — the latter runs synchronously inside
/// `control_loop`, so an unbounded read there could hang the entire bot
/// the same way `prepare_fire`'s reads did before step 34. (copymint's
/// own fetch_public_drop calls run in the copymint watcher task and in
/// axum request handlers — separate tasks, not the synchronous
/// `control_loop` path — so they are not the whole-bot hang this bound
/// exists for.)
const SEADROP_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// STEP 35 — the actual `getPublicDrop` read, bounded by `timeout`. A
/// timeout returns a loud `anyhow` error in `timed_read`'s shape
/// ("getPublicDrop timed out after {ms}ms"), which every caller already
/// handles: main.rs's boot-time `?` (a stalled RPC now fails boot with a
/// clear context instead of hanging forever), main.rs's Prepare-time
/// re-check `match` (logs "mint price re-check failed ... using last known
/// value"), and the copymint/target/delegated callers' existing Err paths —
/// never silence. Inlined here rather than reusing `executor::timed_read`
/// to avoid a seadrop→executor module dependency. Takes `timeout` as a
/// parameter for independent testability with a short duration, same
/// reasoning as `executor`'s timeout helpers.
async fn fetch_public_drop_with_timeout(
    http_rpc: &str,
    seadrop: Address,
    nft_contract: Address,
    timeout: std::time::Duration,
) -> Result<PublicDropInfo> {
    let provider = ProviderBuilder::new().disable_recommended_fillers().connect_http(http_rpc.parse()?);

    let func = Function::parse(
        "getPublicDrop(address) returns (uint80,uint48,uint48,uint16,uint16,bool)",
    )
    .context("parsing getPublicDrop signature")?;

    let calldata = func.abi_encode_input(&[DynSolValue::Address(nft_contract)])?;
    let tx = TransactionRequest::default().to(seadrop).input(calldata.into());

    // `.into_future()` — `provider.call(tx)` is an alloy `IntoFuture`
    // builder, not a `Future` directly; same conversion `executor`'s
    // `timed_read` uses before handing the call to `tokio::time::timeout`.
    let raw = match tokio::time::timeout(timeout, provider.call(tx).into_future()).await {
        Ok(inner) => inner.context("getPublicDrop call failed — check seadrop address and chain")?,
        Err(_elapsed) => anyhow::bail!("getPublicDrop timed out after {}ms", timeout.as_millis()),
    };

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
    use axum::{routing::post, Json, Router};

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

    /// A minimal JSON-RPC mock whose handler never responds — simulates a
    /// stalled TCP/RPC connection with no timeout of its own, the exact
    /// shape STEP 35's fix targets. Sleeps far longer than any timeout used
    /// against it below, so the test genuinely exercises "never responds,"
    /// not "responds slowly." Mirrors executor.rs's own
    /// `spawn_mock_hanging_rpc` (deliberately replicated, not imported —
    /// keeps seadrop.rs free of an executor dependency, same reason the
    /// timeout itself is inlined here).
    async fn spawn_hanging_rpc() -> String {
        let handler = |Json(_body): Json<serde_json::Value>| async move {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Json(serde_json::json!({"jsonrpc": "2.0", "id": 0, "result": "unreachable — timeout should fire first"}))
        };
        let app = Router::new().route("/", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/")
    }

    /// STEP 35 regression test — `fetch_public_drop`'s `provider.call(tx)
    /// .await` had no timeout, and it runs on main.rs's boot-time fetch
    /// and on main.rs's Prepare-time price re-check inside `control_loop`
    /// — synchronous there, so a stalled RPC could hang the entire bot the
    /// same way `prepare_fire`'s reads did before step 34. A mock RPC that
    /// never responds proves the
    /// timeout resolves to a loud, "timed out"-labeled Err within a short
    /// 200ms test timeout (not the real 10s `SEADROP_READ_TIMEOUT`) and
    /// completes well under 2s — same "prove the mechanism, don't make the
    /// test slow" approach as executor.rs's step 33/34 timeout tests.
    /// Reverting the wrapper makes this test hang — the exact regression
    /// it exists to catch.
    #[tokio::test]
    async fn fetch_public_drop_never_hangs_on_a_stalled_rpc() {
        let mock_url = spawn_hanging_rpc().await;
        let seadrop: Address = SEADROP_1_0_MAINNET.parse().unwrap();
        let nft_contract: Address = "0x603a481580c8Cf85ee169b315653bd9D33C39e52".parse().unwrap();

        let started = std::time::Instant::now();
        let result = fetch_public_drop_with_timeout(
            &mock_url,
            seadrop,
            nft_contract,
            std::time::Duration::from_millis(200),
        )
        .await;
        let elapsed = started.elapsed();

        let err = result.expect_err("a stalled getPublicDrop read must resolve to an Err, not hang or silently succeed");
        assert!(
            err.to_string().contains("timed out"),
            "the error should clearly say this was a timeout, not some other failure: {err}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "fetch_public_drop took {elapsed:?} against a 200ms timeout — it did not actually bound the wait, \
             which is the exact regression (step 35's silent bot-wide hang) this test exists to catch"
        );
    }
}
