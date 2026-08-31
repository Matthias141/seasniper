export interface WalletCfg {
  private_key_env: string;
}

export interface Config {
  ws_rpc_url: string;
  http_rpc_urls: string[];
  mint_mode: 'custom' | 'seadrop';
  contract_address: string;
  mint_fn_signature: string;
  mint_fn_args_template: string[];
  mint_state_fn_signature: string;
  trigger_mode: 'poll_state' | 'timestamp' | 'mempool_watch';
  trigger_timestamp_unix: number;
  mint_enable_admin: string;
  seadrop_address: string;
  // The bot's active seadrop target — see StatusBar's TGT pill and
  // TargetResolver.tsx (step 8b). Editable directly here too (existing
  // behavior), or swapped via the resolver's verify-then-confirm flow.
  nft_contract: string;
  fee_recipient: string;
  quantity_per_wallet: number;
  priority_fee_multiplier: number;
  max_priority_fee_gwei_cap: number;
  gas_limit_headroom_pct: number;
  jitter_ms_min: number;
  jitter_ms_max: number;
  gas_jitter_pct: number;
  wallets: WalletCfg[];

  // --- copymint (step 6) ---
  tracked_wallets: string[];
  copymint_auto_fire_free: boolean;
  copymint_auto_fire_paid: boolean;
  max_copymint_price_wei: number;

  // --- target resolution (step 8b/8c) ---
  opensea_api_key_env: string;

  // --- delegated mint mode (v1, DELEGATED_SERIAL) ---
  // Flat fields, not a nested [mint]/[mint.delegated] table — see
  // config.example.toml's own comment on why this deviates from the
  // feature's original spec to match this codebase's real convention.
  // "parallel" (default) leaves the existing race path completely
  // untouched; these three fields are only read when it's "delegated".
  mint_execution: 'parallel' | 'delegated';
  delegate_mnemonic_env: string;
  delegate_count: number;
}

// --- delegated mint mode (v1) — GET /api/delegated/status response.
// Never carries anything beyond public addresses — see
// api.rs's delegated_secrets_tests for the tests asserting this.
export interface DelegatedReceiver {
  index: number;
  address: string;
}

export interface DelegatedStatus {
  operator_address: string;
  operator_balance_eth: string;
  delegate_count: number;
  receivers_derived: boolean;
  max_delegates: number;
  receivers: DelegatedReceiver[];
  mode_label: 'DELEGATED_SERIAL';
}

// POST /api/delegated/preflight response — see delegated/preflight.rs's
// PreflightOutcome. "minter_mismatch" means the contract rejected a
// nonzero minterIfNotPayer specifically — this mode can never be armed
// against that contract, no fallback to parallel mode is offered.
export type DelegatedPreflightResult =
  | { outcome: 'ok'; estimated_max_spend_wei: string; delegate_count: number }
  | { outcome: 'minter_mismatch'; revert_reason: string }
  | { outcome: 'other_failure'; revert_reason: string };

export interface WalletStatus {
  address: string;
  balance_eth: string;
  nonce: number;
  healthy: boolean;
}

export interface StatusResponse {
  armed: boolean;
  wallets: WalletStatus[];
}

export interface OfficialLinks {
  twitter_username: string | null;
  discord_url: string | null;
  instagram_username: string | null;
  telegram_url: string | null;
  project_url: string | null;
}

// --- copymint (step 6) UX: structured skip reasons + eligibility (31b) ---
// Mirrors copymint.rs's CopymintSkipReason exactly — a real, named code
// per skip, not a free-text log line, so the UI can render a specific
// card/badge and (where one exists) a concrete next step instead of
// expecting an operator to parse a log line.
export type CopymintSkipReason =
  | { code: 'drop_lookup_failed'; detail: string }
  | { code: 'not_currently_live'; start_time: number; end_time: number; now: number }
  | { code: 'exceeds_price_ceiling'; total_value_wei: string; ceiling_wei: string }
  | { code: 'calldata_encoding_failed'; detail: string };

export interface CopymintWalletEligibility {
  address: string;
  already_minted: number;
  eligible: boolean;
}

// GET-once-per-click response for the "check eligibility" action —
// see api.rs's post_copymint_eligibility / copymint::check_eligibility.
// A live, real-time estimate (getPublicDrop + getMintStats, never
// cached), not a guarantee — on-chain execution order at fire time is
// what actually decides which wallets land.
export interface CopymintEligibilityReport {
  nft_contract: string;
  max_per_wallet: number;
  current_total_supply: number;
  max_supply: number;
  remaining_supply: number;
  eligible_count: number;
  total_count: number;
  wallets: CopymintWalletEligibility[];
}

export interface SearchHit {
  slug: string;
  name: string;
  image_url: string | null;
  opensea_url: string | null;
}

export interface ResolvedTarget {
  nft_contract: string;
  name: string | null;
  links: OfficialLinks;
  mint_price_wei: string;
  total_value_wei: string;
  quantity_per_wallet: number;
  start_time: number;
  end_time: number;
  max_per_wallet: number;
  restrict_fee_recipients: boolean;
  fee_recipient: string;
  fee_recipient_ok: boolean;
  is_live: boolean;
  settable: boolean;
}

export type ServerEvent =
  | { type: 'log'; level: 'info' | 'warn' | 'error'; message: string; ts: number }
  | { type: 'armed_state'; armed: boolean }
  | { type: 'wallet_update'; address: string; balance_eth: string; nonce: number; healthy: boolean }
  | { type: 'rpc_health'; url: string; healthy: boolean; latency_ms: number }
  | { type: 'trigger_fired'; manual: boolean }
  | {
      type: 'mint_result';
      address: string;
      success: boolean;
      detail: string;
      // step 13e — real, chain-agnostic fire-path timing. trigger_to_dispatch_ms
      // is set on every real fire; send_to_ack_ms/dispatch_to_inclusion_ms are
      // null only when every RPC rejected the broadcast outright (no ack ever
      // happened). Named to match MintDash's own published terms (send→ack,
      // mintDuration) — see CLAUDE.md's step 13 section.
      trigger_to_dispatch_ms: number | null;
      send_to_ack_ms: number | null;
      dispatch_to_inclusion_ms: number | null;
      prepare_age_ms: number;
    }
  | {
      type: 'copy_opportunity';
      tracked_wallet: string;
      nft_contract: string;
      fee_recipient: string;
      mint_price_wei: string;
      total_value_wei: string;
      quantity: number;
      is_free: boolean;
      fireable: boolean;
    }
  | {
      type: 'copymint_skipped';
      tracked_wallet: string;
      nft_contract: string;
      reason: CopymintSkipReason;
      actionable_hint: string | null;
    }
  | { type: 'snapshot'; armed: boolean; wallets: WalletStatus[] }
  // --- delegated mint mode (v1, DELEGATED_SERIAL) --- see bus.rs's
  // ServerEvent doc comment. Never carries a receiver's private key or
  // anything beyond its public address.
  | { type: 'delegated_run_started'; delegate_count: number; estimated_max_spend_wei: string }
  | { type: 'delegated_mint_result'; receiver_index: number; receiver_address: string; success: boolean; detail: string }
  | { type: 'delegated_run_complete'; minted: number; attempted: number; total_cost_wei: string };

// --- identity (step 10) ---
// Mirrors api.rs's get_auth_session response shape exactly — see that
// handler's doc comment for why totp_enrolled/webauthn_enrolled
// (account-wide: "is a factor set up at all") are distinct fields from
// totp_verified/webauthn_verified (this session's own login progress).
export interface AuthSession {
  signed_in: boolean;
  identity_configured: boolean;
  admin_tier?: boolean;
  totp_verified?: boolean;
  webauthn_verified?: boolean;
  totp_enrolled?: boolean;
  webauthn_enrolled?: boolean;
}

export interface TotpSetupMaterial {
  qr_data_uri: string;
  secret_base32: string;
}

export interface WebauthnDevice {
  id: string;
  device_label: string;
  created_at: number;
  last_used_at: number | null;
}
