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

    /// Optional dedicated sequencer JSON-RPC URL. Empty = unset. When set,
    /// `fire_prepared` submits `eth_sendRawTransaction` here first, then
    /// fans out to `http_rpc_urls` as backup. On Robinhood Chain this is
    /// `https://sequencer.{mainnet,testnet}.chain.robinhood.com`.
    #[serde(default)]
    pub sequencer_http_url: String,

    /// Optional WS URL used only by `inclusion::establish_block_ticker`
    /// (post-fire receipt PUSH). Empty = use `ws_rpc_url`. Alchemy (or any
    /// third-party) `eth_subscribe` PUSH means receipt-seen-by-this-RPC,
    /// not sequencer-included. Do NOT point this at the Nitro feed
    /// (`wss://feed.*.chain.robinhood.com`) — that is not an eth WS.
    #[serde(default)]
    pub inclusion_ws_url: String,

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
    /// When true, fire-path jitter must be zero: `validate` fails if
    /// `jitter_ms_max > 0` (and also if min / gas jitter are non-zero).
    /// Default false so existing configs keep their anti-clustering jitter.
    #[serde(default)]
    pub race_mode: bool,
    pub wallets: Vec<WalletCfg>,

    /// --- copymint (step 6) — see src/copymint.rs's doc comment for the
    /// full design: SeaDrop mintPublic only, always-on background watch
    /// independent of trigger_mode/armed state, free/paid split as the
    /// safety boundary. Empty by default — copymint does nothing at all
    /// unless this is populated.
    #[serde(default)]
    pub tracked_wallets: Vec<String>,
    #[serde(default = "default_copymint_auto_fire_free")]
    pub copymint_auto_fire_free: bool,
    #[serde(default)]
    pub copymint_auto_fire_paid: bool,
    #[serde(default)]
    pub max_copymint_price_wei: u64,
    #[serde(default)]
    pub opensea_api_key_env: String,
    #[serde(default)]
    pub google_oauth_client_id: String,
    #[serde(default)]
    pub google_oauth_client_secret_env: String,
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

pub(crate) fn redact_rpc_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => match parsed.host_str() {
            Some(host) => match parsed.port() {
                Some(port) => format!("{}://{host}:{port}/***", parsed.scheme()),
                None => format!("{}://{host}/***", parsed.scheme()),
            },
            None => "<RPC url with no host>".to_string(),
        },
        Err(_) => "<unparseable RPC url>".to_string(),
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading config at {path}"))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        for url in self.rpc_urls_to_validate() {
            let parsed = url::Url::parse(url).with_context(|| format!("invalid RPC url: {url}"))?;
            if !parsed.username().is_empty() || parsed.password().is_some() {
                anyhow::bail!(
                    "RPC url contains embedded credentials (a username/password before the \
                     host) — this codebase's RPC providers authenticate via a path-embedded \
                     key, never via URL userinfo, and alloy's WS client silently turns \
                     userinfo into an Authorization header the provider will likely reject \
                     with an auth error. Remove the `user:pass@` / `user@` prefix from: {url}"
                );
            }
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

        if self.race_mode {
            if self.jitter_ms_max > 0 {
                anyhow::bail!(
                    "race_mode is set but jitter_ms_max is {} — race_mode requires jitter_ms_max = 0 \
                     (fire-path jitter is a self-imposed delay on a ~227ms block time)",
                    self.jitter_ms_max
                );
            }
            if self.jitter_ms_min > 0 {
                anyhow::bail!(
                    "race_mode is set but jitter_ms_min is {} — race_mode requires jitter_ms_min = 0",
                    self.jitter_ms_min
                );
            }
            if self.gas_jitter_pct > 0 {
                anyhow::bail!(
                    "race_mode is set but gas_jitter_pct is {} — race_mode requires gas_jitter_pct = 0",
                    self.gas_jitter_pct
                );
            }
        }

        if self.looks_like_robinhood_chain() && self.block_time_ms == 12_000 {
            anyhow::bail!(
                "this config looks like Robinhood Chain (sequencer_http_url is set or a \
                 chain.robinhood.com host is present) but block_time_ms is still 12000 \
                 (the Ethereum mainnet default). Set block_time_ms to the RH block time \
                 (227 ms) so inclusion polling is not a 12s sleep"
            );
        }

        if self.trigger_mode == "timestamp" && self.trigger_timestamp_unix != 0 {
            let now = bus::now_ts();
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

    fn rpc_urls_to_validate(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.ws_rpc_url)
            .chain(self.http_rpc_urls.iter())
            .chain(std::iter::once(&self.sequencer_http_url).filter(|u| !u.is_empty()))
            .chain(std::iter::once(&self.inclusion_ws_url).filter(|u| !u.is_empty()))
    }

    fn looks_like_robinhood_chain(&self) -> bool {
        let host_is_rh = |u: &str| {
            url::Url::parse(u)
                .ok()
                .and_then(|p| p.host_str().map(str::to_string))
                .map(|h| h.contains("chain.robinhood.com"))
                .unwrap_or_else(|| u.contains("chain.robinhood.com"))
        };
        !self.sequencer_http_url.is_empty()
            || host_is_rh(&self.sequencer_http_url)
            || host_is_rh(&self.ws_rpc_url)
            || host_is_rh(&self.inclusion_ws_url)
            || self.http_rpc_urls.iter().any(|u| host_is_rh(u))
    }

    pub fn block_ticker_ws_url(&self) -> &str {
        if self.inclusion_ws_url.is_empty() {
            &self.ws_rpc_url
        } else {
            &self.inclusion_ws_url
        }
    }

    pub fn resolve_private_keys(&self) -> Result<Vec<String>> {
        self.wallets
            .iter()
            .map(|w| {
                env::var(&w.private_key_env)
                    .with_context(|| format!("env var {} not set", w.private_key_env))
            })
            .collect()
    }

    pub fn resolve_opensea_api_key(&self) -> Option<String> {
        if self.opensea_api_key_env.is_empty() {
            return None;
        }
        env::var(&self.opensea_api_key_env).ok()
    }

    pub fn resolve_google_oauth_client_secret(&self) -> Option<String> {
        if self.google_oauth_client_secret_env.is_empty() {
            return None;
        }
        env::var(&self.google_oauth_client_secret_env).ok()
    }
}

#[cfg(test)]
pub(crate) fn test_config() -> Config {
    Config {
            ws_rpc_url: "wss://eth-mainnet.g.alchemy.com/v2/KEY".to_string(),
            http_rpc_urls: vec!["https://eth-mainnet.g.alchemy.com/v2/KEY".to_string()],
            sequencer_http_url: String::new(),
            inclusion_ws_url: String::new(),
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
            race_mode: false,
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
    fn rejects_ws_rpc_url_with_embedded_userpass_credentials() {
        let mut cfg = test_config();
        cfg.ws_rpc_url = "wss://user:pass@eth-mainnet.g.alchemy.com/v2/KEY".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_ws_rpc_url_with_embedded_username_only() {
        let mut cfg = test_config();
        cfg.ws_rpc_url = "wss://KEY@eth-mainnet.g.alchemy.com/v2/".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_http_rpc_url_with_embedded_credentials() {
        let mut cfg = test_config();
        cfg.http_rpc_urls = vec!["https://user:pass@eth-mainnet.g.alchemy.com/v2/KEY".to_string()];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn redact_rpc_url_strips_the_path_embedded_key() {
        let redacted = redact_rpc_url("wss://eth-mainnet.g.alchemy.com/v2/super-secret-key");
        assert_eq!(redacted, "wss://eth-mainnet.g.alchemy.com/***");
        assert!(!redacted.contains("super-secret-key"));
    }

    #[test]
    fn redact_rpc_url_handles_an_unparseable_url_without_panicking() {
        assert_eq!(redact_rpc_url("not a url at all"), "<unparseable RPC url>");
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
        cfg.block_time_ms = 121_000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_a_fast_chains_block_time_ms() {
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
        let mut cfg = test_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = bus::now_ts() * 1000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_without_copymint_fields_still_parses_with_safe_defaults() {
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
        assert!(cfg.copymint_auto_fire_free);
        assert!(!cfg.copymint_auto_fire_paid);
        assert_eq!(cfg.max_copymint_price_wei, 0);
        assert!(!cfg.race_mode);
        assert!(cfg.sequencer_http_url.is_empty());
        assert!(cfg.inclusion_ws_url.is_empty());
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
        let mut cfg = test_config();
        cfg.trigger_mode = "poll_state".to_string();
        cfg.trigger_timestamp_unix = 1;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_race_mode_with_nonzero_jitter_max() {
        let mut cfg = test_config();
        cfg.race_mode = true;
        cfg.jitter_ms_min = 0;
        cfg.jitter_ms_max = 400;
        cfg.gas_jitter_pct = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("race_mode"), "{err}");
        assert!(err.contains("jitter_ms_max"), "{err}");
    }

    #[test]
    fn accepts_race_mode_with_zero_jitter() {
        let mut cfg = test_config();
        cfg.race_mode = true;
        cfg.jitter_ms_min = 0;
        cfg.jitter_ms_max = 0;
        cfg.gas_jitter_pct = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_robinhood_shaped_config_with_mainnet_block_time() {
        let mut cfg = test_config();
        cfg.sequencer_http_url = "https://sequencer.mainnet.chain.robinhood.com".to_string();
        cfg.block_time_ms = 12_000;
        cfg.inclusion_timeout_ms = 30_000;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("Robinhood"), "{err}");
        assert!(err.contains("12000"), "{err}");
    }

    #[test]
    fn accepts_robinhood_shaped_config_with_fast_block_time() {
        let mut cfg = test_config();
        cfg.sequencer_http_url = "https://sequencer.mainnet.chain.robinhood.com".to_string();
        cfg.http_rpc_urls = vec!["https://rpc.mainnet.chain.robinhood.com".to_string()];
        cfg.block_time_ms = 227;
        cfg.inclusion_timeout_ms = 5_000;
        cfg.race_mode = true;
        cfg.jitter_ms_min = 0;
        cfg.jitter_ms_max = 0;
        cfg.gas_jitter_pct = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn looks_like_robinhood_when_http_rpc_host_is_rh_even_without_sequencer() {
        let mut cfg = test_config();
        cfg.http_rpc_urls = vec!["https://rpc.mainnet.chain.robinhood.com".to_string()];
        cfg.block_time_ms = 12_000;
        assert!(cfg.validate().is_err());
    }
}
