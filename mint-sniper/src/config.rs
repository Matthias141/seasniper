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
    /// SeaDrop singleton contract address. Defaults to the mainnet/Polygon
    /// deployment; override for other chains (verify on that chain's
    /// explorer first — see seadrop.rs doc comment).
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
}

fn default_mint_mode() -> String {
    "custom".to_string()
}
fn default_quantity() -> u64 {
    1
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal, otherwise-valid config to mutate per test — keeps each
    // test focused on the one field it's actually checking.
    fn valid_config() -> Config {
        Config {
            ws_rpc_url: "wss://eth-mainnet.g.alchemy.com/v2/KEY".to_string(),
            http_rpc_urls: vec!["https://eth-mainnet.g.alchemy.com/v2/KEY".to_string()],
            mint_mode: default_mint_mode(),
            contract_address: "0x0000000000000000000000000000000000dEaD".to_string(),
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
        }
    }

    #[test]
    fn accepts_a_valid_config() {
        assert!(valid_config().validate().is_ok());
    }

    #[test]
    fn rejects_malformed_rpc_url() {
        let mut cfg = valid_config();
        cfg.ws_rpc_url = "not a url at all".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_malformed_http_rpc_url() {
        let mut cfg = valid_config();
        cfg.http_rpc_urls = vec!["also not a url".to_string()];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_empty_wallet_list() {
        let mut cfg = valid_config();
        cfg.wallets = vec![];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_negative_priority_fee_multiplier() {
        let mut cfg = valid_config();
        cfg.priority_fee_multiplier = -1.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_negative_max_priority_fee_cap() {
        let mut cfg = valid_config();
        cfg.max_priority_fee_gwei_cap = -0.5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn timestamp_mode_accepts_zero() {
        // 0 means "not configured yet" — distinct from an invalid/past
        // timestamp, and must not be rejected just for being unset.
        let mut cfg = valid_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn timestamp_mode_accepts_plausible_future_time() {
        let mut cfg = valid_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = bus::now_ts() + 3600;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn timestamp_mode_rejects_past_time() {
        let mut cfg = valid_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = bus::now_ts().saturating_sub(3600);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn timestamp_mode_rejects_implausibly_far_future_time() {
        // The classic ms-vs-seconds mistake: a millisecond timestamp
        // pasted into a seconds field lands ~1000x too far out.
        let mut cfg = valid_config();
        cfg.trigger_mode = "timestamp".to_string();
        cfg.trigger_timestamp_unix = bus::now_ts() * 1000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_timestamp_mode_ignores_stale_timestamp_field() {
        // trigger_timestamp_unix is only meaningful in timestamp mode —
        // poll_state/mempool_watch configs shouldn't be rejected over a
        // leftover or stale value in a field they don't use.
        let mut cfg = valid_config();
        cfg.trigger_mode = "poll_state".to_string();
        cfg.trigger_timestamp_unix = 1; // long past, would fail if checked
        assert!(cfg.validate().is_ok());
    }
}
