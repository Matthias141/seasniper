use crate::bus;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WalletCfg {
    pub private_key_env: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    pub ws_rpc_url: String,
    pub http_rpc_urls: Vec<String>,

    /// STEP 14a — the configured chain's expected block time. Used ONLY
    /// to size `inclusion::wait_for_receipt`'s HTTP-polling fallback
    /// interval when the WS push path can't be established — never
    /// assumed to be mainnet's ~12s, which step 13c/14a both found is
    /// badly wrong for a ~100ms-block chain like Robinhood. Defaults to
    /// 12000 (mainnet) for backward compatibility with an existing
    /// config.toml that predates this field; set this explicitly for any
    /// non-mainnet chain. Does NOT affect `run_state_poll_watcher` (step
    /// 13c) at all — that loop reacts to real block-arrival pushes over
    /// WS unconditionally and was deliberately left untouched by 14a; see
    /// that function's own doc comment for why throttling it would be
    /// wrong regardless of this value.
    #[serde(default = "default_block_time_ms")]
    pub block_time_ms: u64,
    /// STEP 14a — hard ceiling on how long `inclusion::wait_for_receipt`
    /// waits for a fired tx's receipt (push or poll path, either one)
    /// before reporting a distinct timed-out result instead of hanging
    /// indefinitely. A tx that's genuinely stuck (underpriced, stuck
    /// mempool, chain congestion) must eventually surface as "we don't
    /// know" rather than block that wallet's fire-completion forever.
    /// Defaults to 30000 (30s) — generous relative to any block time this
    /// codebase currently targets (mainnet's ~12s to Robinhood's ~100ms),
    /// while still bounded.
    #[serde(default = "default_inclusion_timeout_ms")]
    pub inclusion_timeout_ms: u64,

    /// "custom" (default — arbitrary project mint() via mint_fn_signature)
    /// or "seadrop" (fixed ISeaDrop.mintPublic ABI, see src/seadrop.rs).
    #[serde(default = "default_mint_mode")]
    pub mint_mode: String,

    pub contract_address: String,
    pub mint_fn_signature: String,
    pub mint_fn_args_template: Vec<String>,
    pub mint_state_fn_signature: String,
    pub trigger_mode: String,
    pub trigger_timestamp_unix: u64,
    /// Required only for trigger_mode = "mempool_watch": the project's
    /// known admin/owner EOA. The watcher fires the moment it sees a
    /// pending tx FROM this address TO the watched contract (see
    /// watcher.rs's run_mempool_watcher doc comment for exactly which
    /// contract that is in seadrop mode). Empty by default — arming with
    /// trigger_mode = "mempool_watch" and this unset fails loudly rather
    /// than silently falling back to poll_state.
    #[serde(default)]
    pub mint_enable_admin: String,

    /// --- seadrop-mode-only fields, ignored when mint_mode = "custom" ---
    /// SeaDrop singleton contract address. Defaults to
    /// `seadrop::SEADROP_1_0_MAINNET` — despite the name, this same
    /// address is confirmed deployed (step 13a, real `eth_getCode` calls)
    /// on Ethereum mainnet, Polygon, AND Robinhood Chain mainnet+testnet;
    /// override only if targeting a chain not yet confirmed — verify on
    /// that chain's explorer first, see seadrop.rs's doc comment.
    #[serde(default)]
    pub seadrop_address: String,
    /// The actual NFT collection contract for this drop. Distinct from
    /// `contract_address` above: in seadrop mode, all txs go TO the
    /// SeaDrop singleton, with this as an argument, not the tx target.
    #[serde(default)]
    pub nft_contract: String,
    #[serde(default)]
    pub fee_recipient: String,
    #[serde(default = "default_quantity")]
    pub quantity_per_wallet: u64,

    pub priority_fee_multiplier: f64,
    pub max_priority_fee_gwei_cap: f64,
    pub gas_limit_headroom_pct: u64,
    pub jitter_ms_min: u64,
    pub jitter_ms_max: u64,
    pub gas_jitter_pct: u64,
    pub wallets: Vec<WalletCfg>,

    /// --- copymint (step 6) — see src/copymint.rs's doc comment for the
    /// full design: SeaDrop mintPublic only, always-on background watch
    /// independent of trigger_mode/armed state, free/paid split as the
    /// safety boundary. Empty by default — copymint does nothing at all
    /// unless this is populated.
    #[serde(default)]
    pub tracked_wallets: Vec<String>,
    /// Free copymint opportunities (mint_price_wei == 0, as read fresh
    /// from getPublicDrop, never guessed) can auto-fire out of the box —
    /// downside is bounded to gas, already capped by
    /// max_priority_fee_gwei_cap above. Defaults true specifically
    /// because the risk profile is bounded; contrast with
    /// copymint_auto_fire_paid below.
    #[serde(default = "default_copymint_auto_fire_free")]
    pub copymint_auto_fire_free: bool,
    /// Paid copymint opportunities NEVER auto-fire, regardless of this
    /// flag's value — see copymint.rs's `should_auto_fire`, whose
    /// signature structurally cannot see a paid opportunity as
    /// auto-fireable (it doesn't take this field as a parameter at all).
    /// This flag only controls whether the UI offers/enables a one-click
    /// manual fire action for a specific paid opportunity — it is never
    /// read by copymint.rs's watcher or by main.rs's control_loop.
    /// Defaults false: a paid mint spends real ETH on a contract nobody
    /// configured or reviewed in advance, unlike every other trigger
    /// mode, and that needs an explicit opt-in.
    #[serde(default)]
    pub copymint_auto_fire_paid: bool,
    /// Hard ceiling on total ETH value (mint_price_wei * quantity, not
    /// per-token price) for a paid copymint opportunity to be considered
    /// fireable at all — checked independently of copymint_auto_fire_paid
    /// by both copymint.rs (for the `fireable` flag on the emitted event)
    /// and api.rs's manual-fire route (re-verified fresh, not trusted
    /// from a client echo). Defaults to 0, meaning NO paid opportunity is
    /// fireable until this is explicitly raised — the safe default, not
    /// an accidental footgun. u64 wei, same convention as this codebase's
    /// other integer config fields; ~18.4 ETH ceiling headroom, ample for
    /// a sanity cap and avoids the toml crate's lack of native u128
    /// support.
    #[serde(default)]
    pub max_copymint_price_wei: u64,

    /// --- target resolution (step 8b/8c) ---
    /// Env var NAME holding an OpenSea API key — same pattern as
    /// `WalletCfg::private_key_env`: the value round-trips to the UI (it's
    /// just a name), the actual key never does. Needed for resolving an
    /// OpenSea collection URL/slug (8b) or running a name search (8c);
    /// NOT needed for a plain contract address, which needs zero external
    /// calls at all. Empty by default. See opensea.rs's doc comment for
    /// the two ways to obtain a key as of this writing: an instant
    /// self-serve key (expires in 7 days — needs periodic rotation, not
    /// a set-once credential) or the traditional application-form key
    /// (no documented turnaround).
    #[serde(default)]
    pub opensea_api_key_env: String,

    /// --- identity (step 10c) ---
    /// Google Cloud Console OAuth 2.0 Client ID. Not a secret by itself
    /// (Google's own docs treat it as public — it's embedded in the
    /// authorization URL, which is visible to the browser), so it lives
    /// directly in config.toml rather than behind an env-var-name
    /// indirection.
    #[serde(default)]
    pub google_oauth_client_id: String,
    /// Env var NAME holding the OAuth Client Secret — same
    /// name-not-value convention as `WalletCfg::private_key_env` and
    /// `opensea_api_key_env`. This one IS a real secret and must never
    /// appear in config.toml or round-trip to the UI.
    #[serde(default)]
    pub google_oauth_client_secret_env: String,
    /// Full callback URL Google redirects back to. Must exactly match a
    /// Redirect URI registered on the OAuth client in Google Cloud
    /// Console — Google rejects any mismatch. This is also the ONE
    /// canonical origin for this whole instance (WebAuthn's rp_origin
    /// AND the CORS allow-list's public-hostname entry both derive from
    /// it — see `identity::webauthn::derive_origin`'s doc comment) —
    /// step 10.5a's explicit decision, made when Cloudflare Tunnel
    /// access was added, against maintaining two separate live origins.
    /// Two acceptable shapes, pick one:
    /// - `https://<tailscale-magicdns-name>:4117/auth/google/callback` —
    ///   Tailscale-only, nothing reachable off your tailnet. See
    ///   identity/oidc.rs's doc comment for why this needs no public DNS:
    ///   the OAuth server only needs the user's browser to reach it, not
    ///   Google's own backend.
    /// - `https://<your-domain>/auth/google/callback` — a real domain
    ///   fronted by a Cloudflare Tunnel + Access (step 10.5b/c, see
    ///   ui/README.md), for phone reachability with no app install.
    ///   Switching to this from the Tailscale form invalidates every
    ///   existing WebAuthn passkey (origin-bound; see 10.5a) — plan on
    ///   re-registering devices right after the switch, not mid-incident.
    #[serde(default)]
    pub google_oauth_redirect_url: String,
}

fn default_mint_mode() -> String {
    "custom".to_string()
}
fn default_quantity() -> u64 {
    1
}
fn default_copymint_auto_fire_free() -> bool {
    true
}
fn default_block_time_ms() -> u64 {
    12_000
}
fn default_inclusion_timeout_ms() -> u64 {
    30_000
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading config at {path}"))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Sanity checks only — this is not a full semantic validation of
    /// "will this mint actually work" (that needs a real RPC round trip
    /// and can't be done synchronously against arbitrary JSON). The goal
    /// is narrower and cheaper: reject the shapes of bad input that would
    /// otherwise get written to disk silently and only surface as a
    /// confusing failure at arm/fire time, minutes or hours later —
    /// malformed URLs, negative gas knobs, an empty wallet list, or a
    /// timestamp trigger with no plausible trigger time. Called from both
    /// `load()` (startup) and `api::put_config` (every UI save), so a bad
    /// config.toml edited by hand and a bad PUT from the UI get the same
    /// treatment.
    pub fn validate(&self) -> Result<()> {
        for url in std::iter::once(&self.ws_rpc_url).chain(self.http_rpc_urls.iter()) {
            url::Url::parse(url).with_context(|| format!("invalid RPC url: {url}"))?;
        }

        if self.wallets.is_empty() {
            anyhow::bail!("wallets list is empty — at least one wallet is required");
        }

        if self.priority_fee_multiplier < 0.0 {
            anyhow::bail!(
                "priority_fee_multiplier must be non-negative, got {}",
                self.priority_fee_multiplier
            );
        }
        if self.max_priority_fee_gwei_cap < 0.0 {
            anyhow::bail!(
                "max_priority_fee_gwei_cap must be non-negative, got {}",
                self.max_priority_fee_gwei_cap
            );
        }

        // STEP 14a — 0 would make the HTTP-fallback poll loop in
        // inclusion::wait_for_receipt spin with no delay at all, hammering
        // the RPC; an implausibly large value would make the fallback
        // path pointlessly slow to notice a fast chain's inclusion. Same
        // "catch a bad shape at startup/save time" principle as every
        // other check here.
        if self.block_time_ms == 0 {
            anyhow::bail!("block_time_ms must be positive (it sizes the HTTP-fallback inclusion-poll interval)");
        }
        if self.block_time_ms > 120_000 {
            anyhow::bail!(
                "block_time_ms ({}) is implausibly large — check you didn't paste a \
                 seconds value into a milliseconds field",
                self.block_time_ms
            );
        }
        if self.inclusion_timeout_ms == 0 {
            anyhow::bail!("inclusion_timeout_ms must be positive — 0 would time out before ever checking a receipt");
        }
        if self.inclusion_timeout_ms < self.block_time_ms {
            anyhow::bail!(
                "inclusion_timeout_ms ({}) is shorter than block_time_ms ({}) — this would \
                 time out before the fallback poll loop could ever check even once",
                self.inclusion_timeout_ms,
                self.block_time_ms
            );
        }

        if self.trigger_mode == "timestamp" && self.trigger_timestamp_unix != 0 {
            let now = bus::now_ts();
            // Upper bound catches the classic unit mistake (pasting a
            // milliseconds timestamp into a seconds field lands ~1000x in
            // the future — comfortably past this 20-year ceiling) without
            // being so tight it rejects a drop someone is legitimately
            // configuring weeks or months ahead of time.
            const TWENTY_YEARS_SECS: u64 = 20 * 365 * 24 * 60 * 60;
            if self.trigger_timestamp_unix <= now {
                anyhow::bail!(
                    "trigger_timestamp_unix ({}) is not in the future (now is {}) — \
                     trigger_mode = \"timestamp\" needs a real future trigger time, \
                     or 0 if you're not using timestamp mode yet",
                    self.trigger_timestamp_unix,
                    now
                );
            }
            if self.trigger_timestamp_unix > now + TWENTY_YEARS_SECS {
                anyhow::bail!(
                    "trigger_timestamp_unix ({}) is implausibly far in the future — \
                     check you didn't paste a milliseconds timestamp into a seconds field",
                    self.trigger_timestamp_unix
                );
            }
        }

        // All-or-nothing: a config with google_oauth_client_id set but
        // the secret env var name or redirect URL missing (or vice
        // versa) is a half-configured identity setup that would fail
        // confusingly at first-login time rather than at startup/save
        // time — same "reject the bad shape early" principle as every
        // other check in this function.
        let google_fields = [
            !self.google_oauth_client_id.is_empty(),
            !self.google_oauth_client_secret_env.is_empty(),
            !self.google_oauth_redirect_url.is_empty(),
        ];
        if google_fields.iter().any(|f| *f) && !google_fields.iter().all(|f| *f) {
            anyhow::bail!(
                "google_oauth_client_id, google_oauth_client_secret_env, and \
                 google_oauth_redirect_url must all be set together, or all left empty \
                 — partial Google Sign-In configuration would only fail later, at login time"
            );
        }
        if !self.google_oauth_redirect_url.is_empty() {
            url::Url::parse(&self.google_oauth_redirect_url)
                .with_context(|| format!("invalid google_oauth_redirect_url: {}", self.google_oauth_redirect_url))?;
        }

        Ok(())
    }

    /// Resolves each wallet's private key from its configured env var.
    /// Fails loudly (not silently skips) if any var is unset — a missing wallet
    /// at trigger time is worse than a startup crash.
    pub fn resolve_private_keys(&self) -> Result<Vec<String>> {
        self.wallets
            .iter()
            .map(|w| {
                env::var(&w.private_key_env)
                    .with_context(|| format!("env var {} not set", w.private_key_env))
            })
            .collect()
    }

    /// Resolves the OpenSea API key from `opensea_api_key_env`, if
    /// configured. `None` (not an error) when unset or the env var it
    /// names isn't set — unlike wallet keys, missing this isn't fatal to
    /// the whole bot, it just means OpenSea URL/slug resolution and name
    /// search are unavailable until it's configured (a raw contract
    /// address still works with zero external calls either way).
    pub fn resolve_opensea_api_key(&self) -> Option<String> {
        if self.opensea_api_key_env.is_empty() {
            return None;
        }
        env::var(&self.opensea_api_key_env).ok()
    }

    /// Resolves the Google OAuth client secret from
    /// `google_oauth_client_secret_env`. Unlike `resolve_opensea_api_key`,
    /// a configured-but-unresolvable var here is treated as fatal by the
    /// caller (main.rs) — Google Sign-In being half-set-up (client_id
    /// present, secret missing from the environment) should fail loudly
    /// at startup, not silently leave the login route broken.
    pub fn resolve_google_oauth_client_secret(&self) -> Option<String> {
        if self.google_oauth_client_secret_env.is_empty() {
            return None;
        }
        env::var(&self.google_oauth_client_secret_env).ok()
    }
}

/// A minimal, otherwise-valid config — used by this module's own tests
/// (mutated per-test to focus on one field at a time) and reused as-is
/// by other modules' tests that need a real `Config` value without
/// constructing one field-by-field (e.g. `api.rs`'s step-up-auth tests).
#[cfg(test)]
pub(crate) fn test_config() -> Config {
    Config {
            ws_rpc_url: "wss://eth-mainnet.g.alchemy.com/v2/KEY".to_string(),
            http_rpc_urls: vec!["https://eth-mainnet.g.alchemy.com/v2/KEY".to_string()],
            block_time_ms: default_block_time_ms(),
            inclusion_timeout_ms: default_inclusion_timeout_ms(),
            mint_mode: default_mint_mode(),
            contract_address: "0x000000000000000000000000000000000000dEaD".to_string(),
            mint_fn_signature: "mint(uint256)".to_string(),
            mint_fn_args_template: vec!["1".to_string()],
            mint_state_fn_signature: "mintActive()".to_string(),
            trigger_mode: "poll_state".to_string(),
            trigger_timestamp_unix: 0,
            mint_enable_admin: String::new(),
            seadrop_address: String::new(),
            nft_contract: String::new(),
            fee_recipient: String::new(),
            quantity_per_wallet: default_quantity(),
            priority_fee_multiplier: 6.0,
            max_priority_fee_gwei_cap: 15.0,
            gas_limit_headroom_pct: 20,
            jitter_ms_min: 40,
            jitter_ms_max: 400,
            gas_jitter_pct: 8,
            wallets: vec![WalletCfg {
                private_key_env: "SNIPER_PK_1".to_string(),
            }],
            tracked_wallets: Vec::new(),
            copymint_auto_fire_free: default_copymint_auto_fire_free(),
            copymint_auto_fire_paid: false,
            max_copymint_price_wei: 0,
            opensea_api_key_env: String::new(),
            google_oauth_client_id: String::new(),
            google_oauth_client_secret_env: String::new(),
            google_oauth_redirect_url: String::new(),
        }
    }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_valid_config() {
        assert!(test_config().validate().is_ok());
    }

    #[test]
    fn rejects_malformed_rpc_url() {
        let mut cfg = test_config();
        cfg.ws_rpc_url = "not a url at all".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_malformed_http_rpc_url() {
        let mut cfg = test_config();
        cfg.http_rpc_urls = vec!["also not a url".to_string()];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_empty_wallet_list() {
        let mut cfg = test_config();
        cfg.wallets = vec![];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_negative_priority_fee_multiplier() {
        let mut cfg = test_config();
        cfg.priority_fee_multiplier = -1.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_negative_max_priority_fee_cap() {
        let mut cfg = test_config();
        cfg.max_priority_fee_gwei_cap = -0.5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_zero_block_time_ms() {
        let mut cfg = test_config();
        cfg.block_time_ms = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_implausibly_large_block_time_ms() {
        let mut cfg = test_config();
        cfg.block_time_ms = 121_000; // looks like a pasted-seconds value
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_a_fast_chains_block_time_ms() {
        // Robinhood Chain's real ~100ms block time (step 13d) must not
        // trip the "implausibly large" ceiling meant for the OPPOSITE
        // mistake (seconds pasted into a milliseconds field).
        let mut cfg = test_config();
        cfg.block_time_ms = 100;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_zero_inclusion_timeout_ms() {
        let mut cfg = test_config();
        cfg.inclusion_timeout_ms = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_inclusion_timeout_shorter_than_block_time() {
        let mut cfg = test_config();
        cfg.block_time_ms = 12_000;
        cfg.inclusion_timeout_ms = 5_000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn timestamp_mode_accepts_zero() {
        // 0 means "not configured yet" — distinct from an invalid/past
        // timestamp, and must not be rejected just for being unset.
        let mut cfg = test_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn timestamp_mode_accepts_plausible_future_time() {
        let mut cfg = test_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = bus::now_ts() + 3600;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn timestamp_mode_rejects_past_time() {
        let mut cfg = test_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = bus::now_ts().saturating_sub(3600);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn timestamp_mode_rejects_implausibly_far_future_time() {
        // The classic ms-vs-seconds mistake: a millisecond timestamp
        // pasted into a seconds field lands ~1000x too far out.
        let mut cfg = test_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = bus::now_ts() * 1000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_without_copymint_fields_still_parses_with_safe_defaults() {
        // A config.toml written before step 6 (no tracked_wallets,
        // copymint_auto_fire_free/paid, or max_copymint_price_wei at all)
        // must still deserialize — every one of those fields has
        // #[serde(default)]. This is also where the actual default
        // *values* get pinned down as a regression test, not just
        // asserted in a doc comment: free auto-fires (bounded risk, gas
        // only), paid never does until explicitly configured, and the
        // paid ceiling starts at 0 (nothing is fireable until raised).
        let toml_str = r#"
            ws_rpc_url = "wss://example.invalid"
            http_rpc_urls = ["https://example.invalid"]
            contract_address = "0x000000000000000000000000000000000000dEaD"
            mint_fn_signature = "mint(uint256)"
            mint_fn_args_template = ["1"]
            mint_state_fn_signature = "mintActive()"
            trigger_mode = "poll_state"
            trigger_timestamp_unix = 0
            priority_fee_multiplier = 6.0
            max_priority_fee_gwei_cap = 15.0
            gas_limit_headroom_pct = 20
            jitter_ms_min = 40
            jitter_ms_max = 400
            gas_jitter_pct = 8

            [[wallets]]
            private_key_env = "SNIPER_PK_1"
        "#;

        let cfg: Config = toml::from_str(toml_str).expect("old-format config.toml must still parse");
        assert!(cfg.tracked_wallets.is_empty());
        assert!(cfg.copymint_auto_fire_free, "free copymints must default to auto-fireable");
        assert!(!cfg.copymint_auto_fire_paid, "paid copymints must default to NOT auto-fireable");
        assert_eq!(cfg.max_copymint_price_wei, 0, "paid ceiling must default to 0 (nothing fireable until raised)");
    }

    #[test]
    fn google_oauth_fields_all_empty_is_valid() {
        assert!(test_config().validate().is_ok());
    }

    #[test]
    fn google_oauth_fields_all_set_is_valid() {
        let mut cfg = test_config();
        cfg.google_oauth_client_id = "abc.apps.googleusercontent.com".to_string();
        cfg.google_oauth_client_secret_env = "GOOGLE_OAUTH_CLIENT_SECRET".to_string();
        cfg.google_oauth_redirect_url = "https://sniper.tailnet-name.ts.net:4117/auth/google/callback".to_string();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn google_oauth_partial_config_is_rejected() {
        let mut cfg = test_config();
        cfg.google_oauth_client_id = "abc.apps.googleusercontent.com".to_string();
        // secret env and redirect url left empty — must be rejected, not
        // silently accepted and left to fail at login time.
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn google_oauth_bad_redirect_url_is_rejected() {
        let mut cfg = test_config();
        cfg.google_oauth_client_id = "abc.apps.googleusercontent.com".to_string();
        cfg.google_oauth_client_secret_env = "GOOGLE_OAUTH_CLIENT_SECRET".to_string();
        cfg.google_oauth_redirect_url = "not a url".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_timestamp_mode_ignores_stale_timestamp_field() {
        // trigger_timestamp_unix is only meaningful in timestamp mode —
        // poll_state/mempool_watch configs shouldn't be rejected over a
        // leftover or stale value in a field they don't use.
        let mut cfg = test_config();
        cfg.trigger_mode = "poll_state".to_string();
        cfg.trigger_timestamp_unix = 1; // long past, would fail if checked
        assert!(cfg.validate().is_ok());
    }
}
