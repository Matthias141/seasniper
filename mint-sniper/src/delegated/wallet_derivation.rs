//! Delegated mint mode (v1) — BIP-39 mnemonic → operator signer + N
//! receiver addresses, at the standard Ethereum HD path
//! `m/44'/60'/0'/0/{index}`. Index 0 is the operator (the only signer,
//! the only wallet that ever needs funding); indices `1..=delegate_count`
//! are receivers (`mintPublic`'s `minterIfNotPayer` argument — they never
//! sign, never hold gas, never touch a private key operation at all once
//! derived).
//!
//! Uses `alloy::signers::local::MnemonicBuilder` (the `signer-mnemonic`
//! cargo feature, wrapping `coins-bip32`/`coins-bip39`) rather than
//! hand-rolling HD derivation — its default derivation path prefix is
//! exactly `m/44'/60'/0'/0/{index}`, matching this feature's spec without
//! any custom path configuration needed.

use alloy::signers::local::coins_bip39::English;
use alloy::signers::local::{MnemonicBuilder, PrivateKeySigner};
use alloy::primitives::Address;
use anyhow::{Context, Result};
use zeroize::Zeroize;

/// A verified, derived-only set of receiver addresses. **This is the
/// type-level enforcement the feature's own design requires**: `minterIfNotPayer`
/// may only ever be an address WE derived from our own mnemonic — never an
/// arbitrary externally-supplied address (e.g. "mint into a KOL's
/// wallet"). There is no public constructor here that accepts a
/// caller-supplied `Vec<Address>` — the only way to get one of these is
/// `derive_operator_and_receivers` below, which always derives every
/// address itself. A config flag or API parameter accepting a raw
/// address list to mint into is therefore not just discouraged by this
/// module — it has nothing to construct a `DerivedReceiverSet` from.
#[derive(Debug, Clone)]
pub struct DerivedReceiverSet {
    addresses: Vec<Address>,
}

impl DerivedReceiverSet {
    pub fn addresses(&self) -> &[Address] {
        &self.addresses
    }

    pub fn len(&self) -> usize {
        self.addresses.len()
    }

    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }
}

/// Derives the operator signer (index 0) and `delegate_count` receiver
/// addresses (indices `1..=delegate_count`) from `mnemonic`.
///
/// `mnemonic` is taken by value and **zeroized before this function
/// returns, on every path — success or error** (see the closure-wrapped
/// body below: a bare `?` inside a normal function body would return
/// early and skip a zeroize placed after it, which is exactly the bug
/// this shape avoids). The caller (`main.rs`, reading `OPERATOR_MNEMONIC`
/// via `env::var()`) must treat its own copy as consumed after this call
/// — do not retain or reuse it.
///
/// Only the OPERATOR's `PrivateKeySigner` is ever returned. Each
/// receiver's own derived key exists only transiently inside this
/// function's loop — `alloy-signer-local`'s `MnemonicBuilder`/`MnemonicKey`
/// types derive `Zeroize`/`ZeroizeOnDrop` under the "zeroize" feature
/// (pulled in transitively by "signer-mnemonic"), so that key material is
/// cleared when each iteration's temporary values drop, not left to
/// linger in memory as a `Vec<PrivateKeySigner>` this module never builds
/// in the first place.
pub fn derive_operator_and_receivers(
    mut mnemonic: String,
    delegate_count: u32,
) -> Result<(PrivateKeySigner, DerivedReceiverSet)> {
    let result = (|| -> Result<(PrivateKeySigner, DerivedReceiverSet)> {
        if delegate_count == 0 {
            anyhow::bail!("delegate_count must be at least 1 for delegated mint mode");
        }

        // MnemonicBuilder's default derivation_path is "m/44'/60'/0'/0/0";
        // build_parent_key() pops the trailing "/0" and derives the key at
        // "m/44'/60'/0'/0" — confirmed directly against
        // alloy-signer-local's own doc comment on build_parent_key, not
        // assumed. .child(i) from that parent then derives
        // "m/44'/60'/0'/0/{i}", the exact path this feature's spec calls
        // for, with no explicit .index()/.derivation_path() call needed.
        let parent = MnemonicBuilder::<English>::default()
            .phrase(mnemonic.as_str())
            .build_parent_key()
            .context(
                "deriving parent key from OPERATOR_MNEMONIC — check it's a valid BIP-39 phrase",
            )?;

        let operator = parent
            .child(0)
            .context("deriving operator key (index 0)")?
            .signer();

        let mut addresses = Vec::with_capacity(delegate_count as usize);
        for i in 1..=delegate_count {
            let receiver_key = parent
                .child(i)
                .with_context(|| format!("deriving receiver key (index {i})"))?;
            addresses.push(receiver_key.signer().address());
        }

        Ok((operator, DerivedReceiverSet { addresses }))
    })();

    mnemonic.zeroize();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // The standard, universally-public BIP-39 test mnemonic used across
    // every Ethereum tooling test suite (ethers.js, hardhat, foundry, etc.)
    // — NOT a secret, never funded, deliberately chosen so this test file
    // contains no value that could ever be mistaken for a real credential.
    const TEST_MNEMONIC: &str =
        "test test test test test test test test test test test junk";

    #[test]
    fn derives_operator_and_receivers_deterministically() {
        let (op1, recv1) =
            derive_operator_and_receivers(TEST_MNEMONIC.to_string(), 5).expect("derivation must succeed");
        let (op2, recv2) =
            derive_operator_and_receivers(TEST_MNEMONIC.to_string(), 5).expect("derivation must succeed");

        assert_eq!(op1.address(), op2.address(), "operator address must be deterministic");
        assert_eq!(
            recv1.addresses(),
            recv2.addresses(),
            "receiver addresses must be deterministic"
        );
    }

    #[test]
    fn derives_exactly_delegate_count_receivers() {
        let (_, receivers) =
            derive_operator_and_receivers(TEST_MNEMONIC.to_string(), 7).expect("derivation must succeed");
        assert_eq!(receivers.len(), 7);
        assert!(!receivers.is_empty());
    }

    #[test]
    fn operator_and_every_receiver_are_distinct_addresses() {
        let (operator, receivers) =
            derive_operator_and_receivers(TEST_MNEMONIC.to_string(), 10).expect("derivation must succeed");
        let op_addr = operator.address();
        assert!(
            receivers.addresses().iter().all(|r| *r != op_addr),
            "operator (index 0) must never equal any receiver (index 1..N)"
        );
        let mut sorted = receivers.addresses().to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), receivers.len(), "every receiver address must be distinct");
    }

    #[test]
    fn receiver_addresses_match_direct_index_derivation() {
        // Cross-checks the loop in derive_operator_and_receivers against
        // an independent derivation of the SAME indices via
        // MnemonicBuilder::from_phrase_nth — a different code path through
        // the same underlying library, not just "the function agrees with
        // itself."
        let (_, receivers) =
            derive_operator_and_receivers(TEST_MNEMONIC.to_string(), 3).expect("derivation must succeed");
        for (i, addr) in receivers.addresses().iter().enumerate() {
            let index = (i + 1) as u32;
            let expected = MnemonicBuilder::<English>::from_phrase_nth(TEST_MNEMONIC, index);
            assert_eq!(*addr, expected.address(), "receiver at position {i} (HD index {index}) mismatch");
        }
    }

    #[test]
    fn operator_matches_index_zero_direct_derivation() {
        let (operator, _) =
            derive_operator_and_receivers(TEST_MNEMONIC.to_string(), 1).expect("derivation must succeed");
        let expected = MnemonicBuilder::<English>::from_phrase_nth(TEST_MNEMONIC, 0);
        assert_eq!(operator.address(), expected.address());
    }

    #[test]
    fn rejects_zero_delegate_count() {
        assert!(derive_operator_and_receivers(TEST_MNEMONIC.to_string(), 0).is_err());
    }

    #[test]
    fn rejects_an_invalid_mnemonic_phrase() {
        assert!(derive_operator_and_receivers("not a real bip39 phrase".to_string(), 3).is_err());
    }
}
