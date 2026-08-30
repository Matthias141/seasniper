//! Delegated mint mode (v1, MintDash-style) — an opt-in second execution
//! path alongside the existing parallel-EOA one: one funded OPERATOR
//! wallet pays gas + mint price, and NFTs are credited to N unfunded
//! RECEIVER wallets via SeaDrop's `minterIfNotPayer` parameter.
//!
//! **v1 ships operator → N sequential `mintPublic` calls — this is NOT a
//! batched single-transaction mint.** N nonces, N sequencer slots, no
//! helper/factory contract. Every user-visible label for this mode must
//! say `DELEGATED_SERIAL`, never "batch" or "one transaction" — see
//! `executor.rs`'s own doc comment for why this distinction is load-
//! bearing, not just wording.
//!
//! The parallel-EOA path (`wallet.rs`, `executor.rs`) is completely
//! untouched by this module — `mint_execution = "delegated"` in config is
//! the only thing that ever routes execution here, and this module never
//! calls into `executor::prepare_fire`/`fire_prepared` or shares mutable
//! state with them.

pub mod executor;
pub mod preflight;
pub mod wallet_derivation;
