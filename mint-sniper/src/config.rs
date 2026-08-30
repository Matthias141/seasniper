use crate::bus;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct WalletCfg {
    pub private_key_env: String,
}

/// Delegated mint mode (v1) — hardcoded ceiling on `delegate_count`,
/// confirmed with the operator directly before landing this value (not
/// picked unilaterally). Enforced in `Config::validate()`, and mirrored
/// in the UI's capacity indicator (`delegate_count / MAX_DELEGATES
/// active`) — see `ui/src/components/OperatorPanel.tsx`.
pub const MAX_DELEGATES: u32 = 200;

/// Delegated mint mode (v1) — which execution path fires a mint.
/// `Parallel` (default) is the existing, untouched N-funded-wallet race
/// path (`wallet.rs`/`executor.rs`); `Delegated` is the new, opt-in
/// DELEGATED_SERIAL path (`delegated/executor.rs`) — one funded operator
/// wallet, N unfunded receiver wallets credited via SeaDrop's
/// `minterIfNotPayer`. `#[serde(default)]` + the `#[default]` variant
/// attribute below means every config.toml that predates this field
/// still deserializes to `Parallel` with zero behavior change.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MintExecution {
    #[default]
    Parallel,
    Delegated,
}

/// STEP 29d — a structured, read-only VIEW over `Config`'s per-chain
/// tuning fields, grouped in one place. Per step 24d's own finding: with
/// three chains now relevant (Ethereum, Robinhood Chain, InkChain) and
/// `looks_like_robinhood_chain()` already a symptom of these fields being
/// scattered across the flat `Config` struct with no single "here's what
/// differs for this chain" location, the risk of a future chain addition
/// missing a needed value (forgetting `block_time_ms`, the exact class of
/// bug `looks_like_robinhood_chain()`'s own `validate()` check exists to
/// catch after the fact) only grows with each chain added.
///
/// **Deliberately NOT a chain_id-keyed registry/lookup table** — 24d's
/// original sketch described resolving one "from `chain_id` at
/// `validate()` time," but `chain_id` is never trusted from config
/// anywhere in this codebase (confirmed live per-instance via
/// `executor.rs`'s `get_chain_id()` call — step 13b/22c's own explicit,
/// hard-won finding) and `validate()` is synchronous, called from both
/// `Config::load` and every `PUT /api/config` — making it resolve a
/// profile from a live RPC call would be a real, risky architecture
/// change to code that gates every config write, and defeats the whole
/// "never trust a config-provided chain_id" principle if `chain_id` were
/// instead added back as a plain config field just to make this lookup
/// synchronous. This struct is intentionally the smaller, genuinely safe
/// piece of that idea instead: existing flat `Config` fields, unchanged
/// in `config.toml`'s schema (zero behavior change for any existing
/// deployment — confirmed by every existing `Config::validate()` test
/// still passing unmodified), grouped into one coherent, self-documenting
/// struct via `Config::chain_profile()`. A genuinely chain-id-keyed
/// registry remains a real, larger follow-up if ever needed, not
/// something this step attempts.
#[derive(Debug, Clone, PartialEq)]
pub struct ChainProfile {
    pub block_time_ms: u64,
    pub inclusion_timeout_ms: u64,
    pub race_mode: bool,
    /// `None` when unset — mirrors `Config::sequencer_http_url`'s own
    /// "empty string means unset" convention, just typed as `Option` here
    /// since a profile is meant to be read, not round-tripped to TOML.
    pub sequencer_http_url: Option<String>,
    pub jitter_ms_min: u64,
    pub jitter_ms_max: u64,
    pub gas_jitter_pct: u64,
    pub priority_fee_multiplier: f64,
    pub max_priority_fee_gwei_cap: f64,
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

    /// Delegated mint mode (v1) — see `MintExecution`'s own doc comment.
    /// Flat field at the config root, matching every other field in this
    /// struct (`mint_mode`, `trigger_mode`, etc.) — this feature's
    /// original spec proposed a nested `[mint]`/`[mint.delegated]` TOML
    /// table, which does not match this codebase's actual, established
    /// convention (checked directly: `config.example.toml` has no `[mint]`
    /// table anywhere, every field is flat at the root). Adapted to match
    /// what's actually here rather than introducing a new nesting
    /// pattern found nowhere else in this file.
    #[serde(default)]
    pub mint_execution: MintExecution,
    /// Env var NAME holding the BIP-39 mnemonic — same name-not-value
    /// convention as `WalletCfg::private_key_env`/`opensea_api_key_env`/
    /// `google_oauth_client_secret_env`. Only read (via `std::env::var`)
    /// at the moment of firing, inside `delegated::executor::
    /// run_delegated_mint` — never stored, logged, or round-tripped to
    /// the UI/API. Required (and validated below) only when
    /// `mint_execution = "delegated"`.
    #[serde(default)]
    pub delegate_mnemonic_env: String,
    /// How many receiver addresses to derive (HD indices `1..=delegate_count`
    /// — index 0 is always the operator). Bounded by `MAX_DELEGATES` (200,
    /// confirmed with the operator before being hardcoded — see
    /// `delegated/mod.rs`), enforced in `validate()` below, not just
    /// documented here.
    #[serde(default)]
    pub delegate_count: u32,

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

    /// --- clock drift check (step 29a) ---
    /// `trigger_mode = "timestamp"` fires purely off the VPS's own system
    /// clock (`watcher::run_timestamp_watcher` compares `SystemTime::now()`
    /// against `trigger_timestamp_unix` — no RPC involved at all), so a
    /// wrong clock silently mistimes every timestamp-mode arm with nothing
    /// to catch it. HTTPS endpoint used to check clock accuracy at boot
    /// and (for timestamp mode specifically) fresh at every arm — an HTTP
    /// `Date`-header comparison, RTT-compensated, NOT true NTP. Confirmed
    /// this is genuinely simpler and equally reliable for this purpose,
    /// not assumed: step 24's evaluation of `seadrop-noir-bot` found the
    /// same approach there too (their own README's "NTP" framing is
    /// actually this same Date-header technique, confirmed by reading
    /// their source, not their docs) — true NTP needs a UDP client and a
    /// new protocol dependency this codebase has no other use for; a
    /// Date header comes back on every HTTP response for free, reusing
    /// the `reqwest`/`rustls` stack already used everywhere else here.
    /// Empty disables the check entirely. Default is a well-known,
    /// extremely-high-uptime Cloudflare endpoint — Date headers are sent
    /// by every compliant HTTP server, nothing Cloudflare-specific about
    /// the mechanism itself; this default is just a dependable choice,
    /// override it for any other reliable HTTPS host.
    #[serde(default = "default_clock_check_url")]
    pub clock_check_url: String,
    /// Absolute drift (either direction, in ms) at or above this logs a
    /// loud warning via BOTH `bus::log` and `tracing::warn!` — this
    /// project's established "both, not either" standard from step 17's
    /// finding (`bus::log` alone never reaches `journalctl`). 1500ms,
    /// matching seadrop-noir-bot's own threshold — confirmed the same
    /// reasoning applies here, not copied blindly: it sits comfortably
    /// above the Date header's own ~1-second inherent quantization error,
    /// so this doesn't fire on measurement noise alone.
    #[serde(default = "default_clock_drift_warn_ms")]
    pub clock_drift_warn_ms: u64,
    /// STEP 29a — config-gated hard stop. If `trigger_mode = "timestamp"`
    /// and a FRESH drift measurement taken at arm time is at or above this
    /// many ms, refuse to arm at all (log why via both channels, never
    /// spawn the watcher, never flip `armed` to true) instead of only
    /// warning. Defaults to 0 = disabled, on purpose: this is a genuine
    /// hard-failure mode, and an existing deployment upgrading into this
    /// feature must see zero behavior change until an operator explicitly
    /// opts in by setting this. Only meaningful for
    /// `trigger_mode = "timestamp"` — the other trigger modes react to
    /// real on-chain state over RPC, not the local clock, so a drifted
    /// clock doesn't threaten their correctness the same way.
    #[serde(default)]
    pub clock_drift_refuse_arm_ms: u64,
}

fn default_mint_mode() -> String {
    "custom".to_string()
}
fn default_quantity() -> u64 {
    1
}
fn default_clock_check_url() -> String {
    "https://cloudflare.com/cdn-cgi/trace".to_string()
}
fn default_clock_drift_warn_ms() -> u64 {
    1500
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

/// STEP 17b — safe-for-logging form of an RPC URL. `ws_rpc_url`/
/// `http_rpc_urls` embed the provider's API key as a path segment (see
/// `redact_rpc_url`'s callers' doc comments and `Config::validate`'s
/// userinfo check above) — that key must never reach the systemd journal
/// or any other log sink verbatim (this project's standing "secrets never
/// touch a log" rule). This keeps only the scheme and host, which is
/// exactly what's needed to tell WS connection attempts apart in a log
/// (e.g. distinguishing a mainnet vs. testnet endpoint, or catching a
/// typo'd host) without ever printing the key.
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
        for url in self.rpc_urls_to_validate() {
            let parsed = url::Url::parse(url).with_context(|| format!("invalid RPC url: {url}"))?;

            // STEP 17 FINDING — alloy's `WsConnect::new()` silently parses
            // the URL for embedded userinfo credentials
            // (`wss://user:pass@host/...` or `wss://user@host/...`) and,
            // if present, auto-injects an HTTP `Authorization` header into
            // the WebSocket upgrade handshake (alloy-transport-ws's
            // `IntoClientRequest` impl, via `Authorization::extract_from_url`
            // in alloy-transport). A bare WS client (e.g. Node's `ws`)
            // never does this. Every RPC provider this codebase actually
            // targets (Alchemy, and the pattern documented in
            // config.example.toml generally) embeds its auth key as a URL
            // PATH segment, never as URL userinfo — so this codebase never
            // legitimately needs that syntax, and a URL that accidentally
            // contains a stray `@`/`:` before the host (e.g. a corrupted
            // copy-paste) would silently trigger alloy to send a bogus
            // Basic-auth header the provider never expects, which is
            // exactly the shape of failure (an alloy-specific,
            // auth-flavored WS rejection that a bare WS client sending the
            // identical URL does not hit) that motivated this check.
            // Caught here — at load/save time — instead of surfacing only
            // as an opaque "Must be authenticated!" at connect time,
            // hours or days later.
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
            // P0 follow-up 18b — a race_mode config with sequencer_http_url
            // set but http_rpc_urls left empty (the exact shape this
            // feature encourages: race the sequencer, treat backups as
            // optional) used to leave inclusion confirmation structurally
            // impossible whenever the sequencer's own connection wasn't
            // reused for polling. That reuse is fixed in executor.rs now,
            // but a config with BOTH left empty is still unfireable — there
            // would be nothing to broadcast to at all — so reject that
            // shape here rather than let it silently pass validate() and
            // fail confusingly at fire time.
            if self.sequencer_http_url.is_empty() && self.http_rpc_urls.is_empty() {
                anyhow::bail!(
                    "race_mode is set but both sequencer_http_url and http_rpc_urls are empty — \
                     at least one broadcast target is required"
                );
            }
        }

        // STEP 29d — reads block_time_ms via chain_profile() rather than
        // the flat field directly, as a real (not just documented) use
        // site proving the accessor is wired in, not dead code. Identical
        // value either way — chain_profile().block_time_ms is defined as
        // exactly self.block_time_ms, see chain_profile()'s own doc
        // comment and this file's round-trip test.
        if self.looks_like_robinhood_chain() && self.chain_profile().block_time_ms == 12_000 {
            anyhow::bail!(
                "this config looks like Robinhood Chain (sequencer_http_url is set or a \
                 chain.robinhood.com host is present) but block_time_ms is still 12000 \
                 (the Ethereum mainnet default). Set block_time_ms to the RH block time \
                 (227 ms) so inclusion polling is not a 12s sleep"
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

        // Delegated mint mode (v1) — same "catch a bad shape at startup/
        // save time" principle as every other check here. Only enforced
        // when mint_execution = "delegated"; a Parallel config (the
        // default) never even looks at delegate_mnemonic_env/
        // delegate_count, so an existing deployment upgrading into this
        // field's mere existence sees zero new validation constraints.
        if self.mint_execution == MintExecution::Delegated {
            if self.mint_mode != "seadrop" {
                anyhow::bail!(
                    "mint_execution = \"delegated\" requires mint_mode = \"seadrop\" — delegated \
                     mode relies on SeaDrop's minterIfNotPayer parameter, which has no equivalent \
                     in mint_mode = \"custom\""
                );
            }
            if self.delegate_mnemonic_env.is_empty() {
                anyhow::bail!(
                    "mint_execution = \"delegated\" requires delegate_mnemonic_env to be set (the \
                     name of an env var holding the BIP-39 mnemonic — see mint-sniper.env.example)"
                );
            }
            if self.delegate_count == 0 {
                anyhow::bail!("mint_execution = \"delegated\" requires delegate_count >= 1");
            }
            if self.delegate_count > MAX_DELEGATES {
                anyhow::bail!(
                    "delegate_count ({}) exceeds MAX_DELEGATES ({MAX_DELEGATES})",
                    self.delegate_count
                );
            }
        }

        // STEP 29a — same "catch a bad shape at startup/save time" as every
        // other check here. Empty is a deliberate, valid "check disabled"
        // shape (see clock_check_url's own doc comment) — only a
        // non-empty-but-unparseable value is rejected.
        if !self.clock_check_url.is_empty() {
            url::Url::parse(&self.clock_check_url)
                .with_context(|| format!("invalid clock_check_url: {}", self.clock_check_url))?;
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

    /// STEP 29d — see `ChainProfile`'s own doc comment for the full
    /// reasoning. Pure grouping, zero transformation: every field here is
    /// a direct copy of an existing `Config` field, so this cannot change
    /// what any existing caller observes — proven by
    /// `chain_profile_round_trips_every_field` in this file's own tests,
    /// not just asserted in a comment.
    pub fn chain_profile(&self) -> ChainProfile {
        ChainProfile {
            block_time_ms: self.block_time_ms,
            inclusion_timeout_ms: self.inclusion_timeout_ms,
            race_mode: self.race_mode,
            sequencer_http_url: if self.sequencer_http_url.is_empty() {
                None
            } else {
                Some(self.sequencer_http_url.clone())
            },
            jitter_ms_min: self.jitter_ms_min,
            jitter_ms_max: self.jitter_ms_max,
            gas_jitter_pct: self.gas_jitter_pct,
            priority_fee_multiplier: self.priority_fee_multiplier,
            max_priority_fee_gwei_cap: self.max_priority_fee_gwei_cap,
        }
    }

    pub fn block_ticker_ws_url(&self) -> &str {
        if self.inclusion_ws_url.is_empty() {
            &self.ws_rpc_url
        } else {
            &self.inclusion_ws_url
        }
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
            sequencer_http_url: String::new(),
            inclusion_ws_url: String::new(),
            block_time_ms: default_block_time_ms(),
            inclusion_timeout_ms: default_inclusion_timeout_ms(),
            mint_mode: default_mint_mode(),
            mint_execution: MintExecution::default(),
            delegate_mnemonic_env: String::new(),
            delegate_count: 0,
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
            clock_check_url: default_clock_check_url(),
            clock_drift_warn_ms: default_clock_drift_warn_ms(),
            clock_drift_refuse_arm_ms: 0,
        }
    }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_valid_config() {
        assert!(test_config().validate().is_ok());
    }

    // --- STEP 29d: ChainProfile ---

    #[test]
    fn chain_profile_round_trips_every_field() {
        // Proves chain_profile() is a pure grouping with zero
        // transformation — every field matches the source Config field
        // exactly, for a config with every relevant field set to a
        // distinct, non-default value (so a copy-paste mistake between
        // two similarly-named fields, e.g. jitter_ms_min vs.
        // jitter_ms_max, would actually fail this test).
        let mut cfg = test_config();
        cfg.block_time_ms = 227;
        cfg.inclusion_timeout_ms = 9_999;
        cfg.race_mode = false; // kept false: race_mode=true requires jitter fields at 0, which would collide with the distinct-values goal above
        cfg.sequencer_http_url = "https://sequencer.testnet.chain.robinhood.com".to_string();
        cfg.jitter_ms_min = 11;
        cfg.jitter_ms_max = 222;
        cfg.gas_jitter_pct = 3;
        cfg.priority_fee_multiplier = 7.5;
        cfg.max_priority_fee_gwei_cap = 20.0;

        let profile = cfg.chain_profile();
        assert_eq!(profile.block_time_ms, cfg.block_time_ms);
        assert_eq!(profile.inclusion_timeout_ms, cfg.inclusion_timeout_ms);
        assert_eq!(profile.race_mode, cfg.race_mode);
        assert_eq!(profile.sequencer_http_url, Some(cfg.sequencer_http_url.clone()));
        assert_eq!(profile.jitter_ms_min, cfg.jitter_ms_min);
        assert_eq!(profile.jitter_ms_max, cfg.jitter_ms_max);
        assert_eq!(profile.gas_jitter_pct, cfg.gas_jitter_pct);
        assert_eq!(profile.priority_fee_multiplier, cfg.priority_fee_multiplier);
        assert_eq!(profile.max_priority_fee_gwei_cap, cfg.max_priority_fee_gwei_cap);
    }

    #[test]
    fn chain_profile_reports_unset_sequencer_url_as_none() {
        let cfg = test_config();
        assert!(cfg.sequencer_http_url.is_empty(), "precondition: test_config() must default to unset");
        assert_eq!(cfg.chain_profile().sequencer_http_url, None);
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

    // STEP 17 — the actual bug this session spent tonight isolating:
    // alloy's WS client silently turns URL userinfo into an Authorization
    // header a provider like Alchemy never expects. Catch it at
    // validate() time, not as an opaque connect-time auth error.
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
        assert!(!cfg.race_mode, "race_mode must default to false");
        assert!(cfg.sequencer_http_url.is_empty(), "sequencer_http_url must default to unset");
        assert!(cfg.inclusion_ws_url.is_empty(), "inclusion_ws_url must default to unset");
        // STEP 29a
        assert_eq!(cfg.clock_check_url, default_clock_check_url(), "clock_check_url must default to the built-in check endpoint");
        assert_eq!(cfg.clock_drift_warn_ms, 1500, "clock_drift_warn_ms must default to 1500");
        assert_eq!(cfg.clock_drift_refuse_arm_ms, 0, "clock_drift_refuse_arm_ms must default to 0 (disabled) — an existing deployment must see zero behavior change on upgrade");
        // Delegated mint mode (v1) — an old config predating this field
        // entirely must still parse, and mint_execution must default to
        // Parallel: the existing, untouched race path, not a mode change
        // no operator asked for.
        assert_eq!(cfg.mint_execution, MintExecution::Parallel, "mint_execution must default to Parallel");
        assert!(cfg.delegate_mnemonic_env.is_empty(), "delegate_mnemonic_env must default to unset");
        assert_eq!(cfg.delegate_count, 0, "delegate_count must default to 0");
        assert!(cfg.validate().is_ok(), "an old, Parallel-mode config must still validate cleanly");
    }

    // --- Delegated mint mode (v1) ---

    #[test]
    fn delegated_mode_requires_mnemonic_env_and_delegate_count() {
        let mut cfg = test_config();
        cfg.mint_mode = "seadrop".to_string();
        cfg.mint_execution = MintExecution::Delegated;
        // delegate_mnemonic_env and delegate_count both still unset/0.
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("delegate_mnemonic_env"), "{err}");
    }

    #[test]
    fn delegated_mode_requires_seadrop_mint_mode() {
        let mut cfg = test_config();
        cfg.mint_mode = "custom".to_string();
        cfg.mint_execution = MintExecution::Delegated;
        cfg.delegate_mnemonic_env = "OPERATOR_MNEMONIC".to_string();
        cfg.delegate_count = 5;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("mint_mode"), "{err}");
    }

    #[test]
    fn delegated_mode_rejects_zero_delegate_count() {
        let mut cfg = test_config();
        cfg.mint_mode = "seadrop".to_string();
        cfg.mint_execution = MintExecution::Delegated;
        cfg.delegate_mnemonic_env = "OPERATOR_MNEMONIC".to_string();
        cfg.delegate_count = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn delegated_mode_rejects_delegate_count_over_max() {
        let mut cfg = test_config();
        cfg.mint_mode = "seadrop".to_string();
        cfg.mint_execution = MintExecution::Delegated;
        cfg.delegate_mnemonic_env = "OPERATOR_MNEMONIC".to_string();
        cfg.delegate_count = MAX_DELEGATES + 1;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("MAX_DELEGATES") || err.contains("exceeds"), "{err}");
    }

    #[test]
    fn delegated_mode_accepts_a_well_formed_config() {
        let mut cfg = test_config();
        cfg.mint_mode = "seadrop".to_string();
        cfg.mint_execution = MintExecution::Delegated;
        cfg.delegate_mnemonic_env = "OPERATOR_MNEMONIC".to_string();
        cfg.delegate_count = MAX_DELEGATES; // exactly at the ceiling — must be accepted, not rejected
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn parallel_mode_ignores_unset_delegated_fields_entirely() {
        // The default MintExecution::Parallel with delegate_mnemonic_env
        // empty and delegate_count 0 must validate cleanly — this is
        // exactly what every existing config.toml looks like today.
        let cfg = test_config();
        assert_eq!(cfg.mint_execution, MintExecution::Parallel);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_malformed_clock_check_url() {
        let mut cfg = test_config();
        cfg.clock_check_url = "not a url at all".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn empty_clock_check_url_is_valid_and_means_disabled() {
        let mut cfg = test_config();
        cfg.clock_check_url = String::new();
        assert!(cfg.validate().is_ok());
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
