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
}

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
  | { type: 'mint_result'; address: string; success: boolean; detail: string }
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
  | { type: 'snapshot'; armed: boolean; wallets: WalletStatus[] };
