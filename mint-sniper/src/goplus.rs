//! STEP 29b — GoPlus NFT Security API integration: an ADDITIONAL warning
//! signal in target resolution, never a hard block and never a
//! replacement for human judgment, same spirit as the existing
//! namesquatting warning in `opensea.rs`'s search flow (8c).
//!
//! Confirmed real, free, and accessible before building this, not
//! assumed — step 24's evaluation of `seadrop-noir-bot` found the same
//! endpoint (`nft_security/{chain_id}`) already wired into a real, live
//! validation path there, corroborating this is a genuinely usable public
//! API. Independently re-confirmed live this session with a real request
//! against BAYC's mainnet contract before writing a single line of
//! parsing logic against it — the actual response schema was checked
//! directly, not assumed from the endpoint name or from
//! `seadrop-noir-bot`'s own summary of it. That response has NO
//! `is_honeypot` field at all (that field belongs to GoPlus's separate
//! token/address security product, a different endpoint) — this
//! integration therefore does NOT claim to check for one, unlike this
//! step's original framing; see `NftSecurityCheck`'s own doc comment for
//! exactly what IS checked and why.
//!
//! **Deliberately conservative about which fields this module interprets
//! as a risk signal.** The real response also carries `privileged_minting`
//! / `privileged_burn` / `self_destruct` / `transfer_without_approval`
//! object fields with a `value: -1/0/1` encoding whose exact semantics
//! could not be confirmed against GoPlus's own documentation in the time
//! available (their docs page is JS-rendered; static fetches returned no
//! field-level detail). Guessing wrong here would be worse than not using
//! them at all — a false "safe" reading is actively dangerous, and a
//! false "risky" reading erodes trust in every future warning this
//! feature raises. Only `malicious_nft_contract` (an explicit,
//! self-describing 0/1 flag — no interpretation needed) and
//! `create_block_number` (a block number, equally unambiguous) are used.
//! Revisit if GoPlus's docs become fetchable, or their support channel
//! confirms the other fields' encoding.

use alloy::primitives::Address;
use anyhow::Result;
use serde::Deserialize;

const BASE_URL: &str = "https://api.gopluslabs.io/api/v1/nft_security";

/// `None` on any field means "not determined" — a network error, an
/// unsupported chain (GoPlus does not yet cover Robinhood Chain, per step
/// 24's own finding), or a field GoPlus simply didn't return for this
/// contract. Never conflate `None` with "confirmed clean" at a call site.
#[derive(Debug, Clone, Default)]
pub struct NftSecurityCheck {
    pub malicious_nft_contract: Option<bool>,
    pub create_block_number: Option<u64>,
}

impl NftSecurityCheck {
    pub fn is_concerning(&self) -> bool {
        self.malicious_nft_contract == Some(true)
    }
}

#[derive(Deserialize)]
struct ApiResponse {
    code: i64,
    result: Option<ApiResult>,
}

#[derive(Deserialize, Default)]
struct ApiResult {
    #[serde(default)]
    malicious_nft_contract: Option<i64>,
    #[serde(default)]
    create_block_number: Option<u64>,
}

/// Fails open, always — a GoPlus outage or an unsupported chain must
/// never block target resolution; this is an additional signal on top of
/// the existing on-chain verification (`getPublicDrop`), not a
/// replacement for it. Returns `Ok(NftSecurityCheck::default())` (all
/// `None`) on any failure, never `Err` — callers don't need a
/// success/failure branch at all, only whichever fields came back `Some`.
pub async fn check(http_client: &reqwest::Client, chain_id: u64, address: Address) -> NftSecurityCheck {
    match check_inner(http_client, chain_id, address).await {
        Ok(check) => check,
        Err(e) => {
            tracing::warn!(%address, chain_id, error = %e, "goplus nft_security check failed — treating as unchecked, not blocking resolution");
            NftSecurityCheck::default()
        }
    }
}

async fn check_inner(http_client: &reqwest::Client, chain_id: u64, address: Address) -> Result<NftSecurityCheck> {
    let url = format!("{BASE_URL}/{chain_id}");
    let resp: ApiResponse = http_client
        .get(&url)
        .query(&[("contract_addresses", format!("{address:#x}"))])
        .send()
        .await?
        .json()
        .await?;

    if resp.code != 1 {
        // A non-1 code is GoPlus's own documented "not ok" signal (e.g.
        // chain not supported) — not a Rust-level parse error, so this
        // returns Ok(default) via check_inner's caller rather than
        // bailing, same fail-open treatment as any other unchecked case.
        return Ok(NftSecurityCheck::default());
    }
    let result = resp.result.unwrap_or_default();
    Ok(NftSecurityCheck {
        malicious_nft_contract: result.malicious_nft_contract.map(|v| v != 0),
        create_block_number: result.create_block_number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchecked_is_never_concerning() {
        assert!(!NftSecurityCheck::default().is_concerning(), "None must never read as a risk signal");
    }

    #[test]
    fn confirmed_clean_is_not_concerning() {
        let check = NftSecurityCheck { malicious_nft_contract: Some(false), create_block_number: Some(1) };
        assert!(!check.is_concerning());
    }

    #[test]
    fn confirmed_malicious_is_concerning() {
        let check = NftSecurityCheck { malicious_nft_contract: Some(true), create_block_number: None };
        assert!(check.is_concerning());
    }

    #[test]
    fn api_response_parses_the_real_confirmed_live_schema() {
        // The actual shape returned by a real, live GET to
        // https://api.gopluslabs.io/api/v1/nft_security/1?contract_addresses=...
        // (BAYC's mainnet contract), captured this session, trimmed to
        // just the fields this module reads — confirms this module's
        // parsing matches GoPlus's real response, not a guessed schema.
        let body = r#"{"code":1,"message":"ok","result":{"malicious_nft_contract":0,"create_block_number":12287507,"nft_verified":1}}"#;
        let parsed: ApiResponse = serde_json::from_str(body).expect("must parse the real captured schema");
        assert_eq!(parsed.code, 1);
        let result = parsed.result.expect("result must be present");
        assert_eq!(result.malicious_nft_contract, Some(0));
        assert_eq!(result.create_block_number, Some(12287507));
    }

    #[test]
    fn api_response_tolerates_a_missing_create_block_number() {
        // A real, live-observed case (a different collection than the
        // fixture above) — GoPlus doesn't always have this field
        // populated, and this module must not error out over its absence.
        let body = r#"{"code":1,"message":"ok","result":{"malicious_nft_contract":0}}"#;
        let parsed: ApiResponse = serde_json::from_str(body).expect("missing optional fields must still parse");
        let result = parsed.result.expect("result must be present");
        assert_eq!(result.create_block_number, None);
    }
}
