//! Delegated mint mode (v1) — mandatory pre-arm gate. Nothing in
//! `executor.rs` may fire without this having run and returned
//! `PreflightOutcome::Ok` first.

use crate::seadrop;
use alloy::dyn_abi::{DynSolValue, JsonAbiExt};
use alloy::json_abi::Function;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use anyhow::{Context, Result};

/// Distinguishes "the contract specifically rejects/ignores a nonzero
/// `minterIfNotPayer`" (refuse to arm, unconditionally — never silently
/// fall back to parallel mode) from any other preflight failure (stage
/// not live yet, insufficient value, etc — a normal, different, and
/// separately-reported problem).
#[derive(Debug, Clone, PartialEq)]
pub enum PreflightOutcome {
    Ok { estimated_max_spend_wei: U256 },
    MinterMismatch { revert_reason: String },
    OtherFailure { revert_reason: String },
}

/// Runs the full preflight: simulates (never sends) a `mintPublic` call
/// with a real derived receiver as `minterIfNotPayer`, classifies any
/// failure, and computes the estimated max spend.
///
/// **How `MinterMismatch` is actually detected — a real comparison, not a
/// guessed string match.** SeaDrop's own audited `mintPublic` supports
/// `minterIfNotPayer` unconditionally by design (see `seadrop.rs`'s own
/// doc comment) — the case this exists to catch is a project's own
/// contract layered in front of the singleton behaving differently.
/// Rather than pattern-matching arbitrary revert strings across arbitrary
/// contracts (which would mean guessing at error shapes this session has
/// no way to enumerate), this simulates BOTH the real delegated call
/// (`minterIfNotPayer = receiver`) AND the same call with
/// `minterIfNotPayer = Address::ZERO` (SeaDrop's own "mint to
/// msg.sender" convention — the one thing already independently
/// confirmed working via `getPublicDrop`/the parallel path's own
/// behavior). If the ZERO variant succeeds but the receiver variant
/// reverts, the delta IS the `minterIfNotPayer` value — real, isolated
/// evidence, not an inference from error text. If BOTH revert, the drop
/// itself has an unrelated problem (classified `OtherFailure`), same as
/// what the parallel path would also hit. Revert reasons are surfaced via
/// the RPC error's own `Display` formatting, verbatim — no further ABI
/// revert-decoding is attempted, matching this codebase's existing,
/// explicit choice not to do that anywhere else (see `CLAUDE.md`'s gap #7
/// note on `fire_prepared`'s own revert handling).
#[allow(clippy::too_many_arguments)]
pub async fn run_preflight(
    http_rpc_url: &str,
    seadrop_address: Address,
    nft_contract: Address,
    fee_recipient: Address,
    operator: Address,
    receiver: Address,
    quantity_per_wallet: u64,
    delegate_count: u32,
) -> Result<PreflightOutcome> {
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(http_rpc_url.parse()?);

    let drop = seadrop::fetch_public_drop(http_rpc_url, seadrop_address, nft_contract)
        .await
        .context("getPublicDrop failed during delegated preflight")?;

    let total_value_per_call =
        drop.mint_price_wei.saturating_mul(U256::from(quantity_per_wallet));

    let zero_calldata = crate::seadrop::encode_mint_public(nft_contract, fee_recipient, quantity_per_wallet)
        .context("building the minterIfNotPayer=ZERO comparison calldata")?;
    let delegated_calldata = encode_mint_public_delegated(
        nft_contract,
        fee_recipient,
        receiver,
        quantity_per_wallet,
    )
    .context("building the real delegated calldata")?;

    let zero_result = provider
        .call(
            TransactionRequest::default()
                .to(seadrop_address)
                .from(operator)
                .input(zero_calldata.into())
                .value(total_value_per_call),
        )
        .await;

    let delegated_result = provider
        .call(
            TransactionRequest::default()
                .to(seadrop_address)
                .from(operator)
                .input(delegated_calldata.clone().into())
                .value(total_value_per_call),
        )
        .await;

    match (zero_result, delegated_result) {
        (Ok(_), Err(delegated_err)) => Ok(PreflightOutcome::MinterMismatch {
            revert_reason: format!("{delegated_err:#}"),
        }),
        (Err(zero_err), Err(_)) => Ok(PreflightOutcome::OtherFailure {
            revert_reason: format!("{zero_err:#}"),
        }),
        (Err(zero_err), Ok(_)) => {
            // Genuinely shouldn't happen (the ZERO-recipient case is the
            // strictly more standard one) — reported as OtherFailure
            // rather than silently treated as Ok, since something is
            // still inconsistent about this drop's state.
            Ok(PreflightOutcome::OtherFailure {
                revert_reason: format!(
                    "inconsistent preflight result (minterIfNotPayer=ZERO reverted but the \
                     real delegated call succeeded) — investigate before arming: {zero_err:#}"
                ),
            })
        }
        (Ok(_), Ok(_)) => {
            let gas_estimate = provider
                .estimate_gas(
                    TransactionRequest::default()
                        .to(seadrop_address)
                        .from(operator)
                        .input(delegated_calldata.into())
                        .value(total_value_per_call),
                )
                .await
                .context("estimating gas for the delegated call")?;
            let gas_price = provider.get_gas_price().await.context("fetching gas price")?;
            let gas_cost_per_call_wei = U256::from(gas_estimate).saturating_mul(U256::from(gas_price));

            let estimated_max_spend_wei = total_value_per_call
                .saturating_mul(U256::from(delegate_count))
                .saturating_add(gas_cost_per_call_wei.saturating_mul(U256::from(delegate_count)));

            Ok(PreflightOutcome::Ok { estimated_max_spend_wei })
        }
    }
}

/// STEP: delegated mint mode. Same calldata shape as
/// `seadrop::encode_mint_public`, but with an explicit, non-zero
/// `minterIfNotPayer` — kept as a SEPARATE function rather than a new
/// parameter added to the existing one, specifically so the parallel
/// path's already-verified calldata (and its own byte-for-byte test) can
/// never accidentally regress from a change made for this feature.
pub fn encode_mint_public_delegated(
    nft_contract: Address,
    fee_recipient: Address,
    minter_if_not_payer: Address,
    quantity: u64,
) -> Result<Vec<u8>> {
    let func = Function::parse("mintPublic(address,address,address,uint256)")
        .context("parsing mintPublic signature")?;
    let values = vec![
        DynSolValue::Address(nft_contract),
        DynSolValue::Address(fee_recipient),
        DynSolValue::Address(minter_if_not_payer),
        DynSolValue::Uint(U256::from(quantity), 256),
    ];
    Ok(func.abi_encode_input(&values)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegated_calldata_sets_a_nonzero_correctly_mapped_minter() {
        let nft_contract: Address = "0x1234567890123456789012345678901234567890".parse().unwrap();
        let fee_recipient: Address = "0x0000a26b00c1F0DF003000390027140000fAa719".parse().unwrap();
        let receiver: Address = "0x0000000000000000000000000000000000AbCd12".parse().unwrap();

        let calldata = encode_mint_public_delegated(nft_contract, fee_recipient, receiver, 1).unwrap();

        // Built independently of encode_mint_public_delegated's own logic
        // — same cross-check discipline as seadrop.rs's own
        // encode_mint_public test. Selector 0x161ac21f is
        // seadrop.rs::encode_mint_public's own already-verified value for
        // this exact function signature (mintPublic(address,address,
        // address,uint256)) — a selector depends only on the signature,
        // never the argument values, so it's identical here.
        let mut expected: Vec<u8> = vec![0x16, 0x1a, 0xc2, 0x1f];
        expected.extend_from_slice(nft_contract.into_word().as_slice());
        expected.extend_from_slice(fee_recipient.into_word().as_slice());
        expected.extend_from_slice(receiver.into_word().as_slice()); // minterIfNotPayer — the real, non-zero receiver
        expected.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>()); // quantity = 1

        assert_eq!(calldata, expected);
        // The receiver's 20 bytes must appear at the minterIfNotPayer
        // slot, never Address::ZERO there.
        assert_ne!(&calldata[68..100], &[0u8; 32][..]);
    }

    #[test]
    fn delegated_calldata_differs_from_zero_minter_only_in_the_minter_field() {
        let nft_contract: Address = "0x1234567890123456789012345678901234567890".parse().unwrap();
        let fee_recipient: Address = "0x0000a26b00c1F0DF003000390027140000fAa719".parse().unwrap();
        let receiver: Address = "0x0000000000000000000000000000000000AbCd12".parse().unwrap();

        let zero = seadrop::encode_mint_public(nft_contract, fee_recipient, 1).unwrap();
        let delegated = encode_mint_public_delegated(nft_contract, fee_recipient, receiver, 1).unwrap();

        assert_eq!(zero.len(), delegated.len());
        // Bytes 68..100 are the minterIfNotPayer word — must be the only
        // difference between the two encodings.
        assert_ne!(zero[68..100], delegated[68..100]);
        assert_eq!(zero[..68], delegated[..68], "selector + nftContract + feeRecipient must match exactly");
        assert_eq!(zero[100..], delegated[100..], "quantity must match exactly");
    }
}
