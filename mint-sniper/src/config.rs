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
