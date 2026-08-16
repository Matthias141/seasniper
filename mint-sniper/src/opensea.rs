//! OpenSea integration for step 8: resolving a pasted address/URL (8b)
//! and free-text collection search (8c) into a verifiable target
//! candidate. Talks to OpenSea's REST API (v2), not the SeaDrop contract
//! itself — nothing in this module decides anything is safe to mint
//! from. Every result here is an unverified candidate until it goes
//! through `seadrop::fetch_public_drop`, the actual on-chain source of
//! truth, in `api.rs`'s resolve/set handlers.
//!
//! API KEY REALITY CHECK — checked directly against docs.opensea.io
//! while building this, not assumed from an earlier audit. OpenSea's v2
//! `/collections/{slug}` endpoint (needed for URL/slug resolution, 8b)
//! now requires an API key (`x-api-key` header), same as `/search` (8c).
//! This differs from this feature's original framing ("8b's slug/URL
//! resolution... needs none") — that premise is out of date, flagged
//! explicitly here rather than silently built around. A plain `0x`
//! address still needs zero external calls at all — `parse_input` below
//! returns `Address` directly with no network round trip; only URL/slug/
//! search input actually needs the key.
//!
//! KEY REQUEST PROCESS — also checked directly
//! (docs.opensea.io/reference/api-keys), not assumed still the old
//! multi-day-manual-only process. OpenSea now also offers an INSTANT
//! self-serve key (one API call, no signup) — but it expires after 7
//! days and caps at 600 reads/hour. The traditional application-form key
//! has no documented turnaround. See `opensea_api_key_env`'s doc comment
//! in `config.rs`: an instant key is not a "set once and forget"
//! credential.

use alloy::primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

const OPENSEA_API_BASE: &str = "https://api.opensea.io/api/v2";

/// What a pasted input resolved to, before any network call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedInput {
    /// A raw `0x...` address — used directly, no OpenSea call needed at
    /// all, regardless of whether an API key is configured.
    Address(Address),
    /// An OpenSea collection slug, extracted from a pasted URL. Needs
    /// `resolve_slug` (and therefore an API key) to become an address.
    OpenSeaSlug(String),
}

/// Parses pasted input into either a raw address or an OpenSea slug.
/// Returns `None` for anything that's neither — e.g. free text, which is
/// 8c's search box's job, not this function's.
pub fn parse_input(input: &str) -> Option<ResolvedInput> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(addr) = trimmed.parse::<Address>() {
        return Some(ResolvedInput::Address(addr));
    }
    // https://opensea.io/collection/{slug}[/...] or [?...] — the slug is
    // exactly the path segment right after "/collection/".
    if let Some(idx) = trimmed.find("/collection/") {
        let after = &trimmed[idx + "/collection/".len()..];
        let slug = after.split(['/', '?', '#']).next().unwrap_or("").trim();
        if !slug.is_empty() {
            return Some(ResolvedInput::OpenSeaSlug(slug.to_string()));
        }
    }
    None
}

/// Official links a project put on their own OpenSea collection page —
/// surfaced so the operator can go verify on X/Discord/etc themselves,
/// using their own judgment. Never used by this codebase to make a
/// decision on its own; see 8c's reasoning in CLAUDE.md for why this
/// project deliberately does not attempt to search or scrape X directly.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct OfficialLinks {
    pub twitter_username: Option<String>,
    pub discord_url: Option<String>,
    pub instagram_username: Option<String>,
    pub telegram_url: Option<String>,
    pub project_url: Option<String>,
}

pub struct ResolvedCollection {
    pub nft_contract: Address,
    pub name: String,
    pub links: OfficialLinks,
}

#[derive(Deserialize)]
struct GetCollectionResponse {
    #[serde(default)]
    name: String,
    #[serde(default)]
    contracts: Vec<ContractEntry>,
    #[serde(default)]
    twitter_username: Option<String>,
    #[serde(default)]
    discord_url: Option<String>,
    #[serde(default)]
    instagram_username: Option<String>,
    #[serde(default)]
    telegram_url: Option<String>,
    #[serde(default)]
    project_url: Option<String>,
}

#[derive(Deserialize)]
struct ContractEntry {
    address: String,
    #[serde(default)]
    chain: String,
}

/// Resolves an OpenSea collection slug to its NFT contract address plus
/// official links, via `GET /v2/collections/{slug}`. Requires an API
/// key — returns a clear error (not a panic, not a silent empty result)
/// if none is configured, per this file's doc comment.
pub async fn resolve_slug(client: &reqwest::Client, api_key: Option<&str>, slug: &str) -> Result<ResolvedCollection> {
    let api_key = api_key.context(
        "resolving an OpenSea collection URL needs an OpenSea API key (opensea_api_key_env in \
         config is unset or the env var it names isn't set) — paste the raw contract address \
         instead, or configure a key",
    )?;

    let url = format!("{OPENSEA_API_BASE}/collections/{slug}");
    let resp = client
        .get(&url)
        .header("X-API-KEY", api_key)
        .send()
        .await
        .context("calling OpenSea's get-collection endpoint")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenSea get-collection returned {status}: {body}");
    }

    let parsed: GetCollectionResponse = resp
        .json()
        .await
        .context("parsing OpenSea get-collection response")?;

    // Prefer an Ethereum mainnet contract entry if the collection is
    // multi-chain; otherwise take whatever's first. Chain selection
    // beyond "prefer ethereum" is out of scope — this bot targets
    // Ethereum-family EVM chains via whatever http_rpc_urls points at,
    // and picking the wrong chain's contract here would just fail
    // getPublicDrop's verification loudly, not silently mint wrong.
    let contract_entry = parsed
        .contracts
        .iter()
        .find(|c| c.chain.eq_ignore_ascii_case("ethereum"))
        .or_else(|| parsed.contracts.first())
        .context("OpenSea collection has no contracts listed")?;

    let nft_contract: Address = contract_entry
        .address
        .parse()
        .with_context(|| format!("OpenSea returned an unparseable contract address: {}", contract_entry.address))?;

    Ok(ResolvedCollection {
        nft_contract,
        name: parsed.name,
        links: OfficialLinks {
            twitter_username: parsed.twitter_username,
            discord_url: parsed.discord_url,
            instagram_username: parsed.instagram_username,
            telegram_url: parsed.telegram_url,
            project_url: parsed.project_url,
        },
    })
}

// Free-text collection search (step 8c: SearchHit, search_collections,
// and their response types) lands in its own commit, deliberately not
// here — see this file's tests and CLAUDE.md for why 8c is scoped as a
// separate, more cautious feature than 8b's direct resolve.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_recognizes_a_raw_address() {
        let addr = "0x603a481580c8Cf85ee169b315653bd9D33C39e52";
        match parse_input(addr) {
            Some(ResolvedInput::Address(a)) => assert_eq!(a, addr.parse::<Address>().unwrap()),
            other => panic!("expected Address, got {other:?}"),
        }
    }

    #[test]
    fn parse_input_extracts_slug_from_a_plain_collection_url() {
        match parse_input("https://opensea.io/collection/everybodys") {
            Some(ResolvedInput::OpenSeaSlug(s)) => assert_eq!(s, "everybodys"),
            other => panic!("expected OpenSeaSlug, got {other:?}"),
        }
    }

    #[test]
    fn parse_input_extracts_slug_ignoring_trailing_path_and_query() {
        match parse_input("https://opensea.io/collection/everybodys/overview?tab=items") {
            Some(ResolvedInput::OpenSeaSlug(s)) => assert_eq!(s, "everybodys"),
            other => panic!("expected OpenSeaSlug, got {other:?}"),
        }
    }

    #[test]
    fn parse_input_rejects_free_text() {
        // 8c's job, not this function's — an ambiguous name search input
        // must not be silently treated as a slug or address.
        assert_eq!(parse_input("cool cats nft"), None);
        assert_eq!(parse_input(""), None);
    }
}
