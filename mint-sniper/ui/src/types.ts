export interface WalletCfg {
  private_key_env: string;
}

export interface Config {
  ws_rpc_url: string;
  http_rpc_urls: string[];
  contract_address: string;
  mint_fn_signature: string;
  mint_fn_args_template: string[];
  mint_state_fn_signature: string;
  trigger_mode: 'poll_state' | 'timestamp' | 'mempool_watch';
  trigger_timestamp_unix: number;
  mint_enable_admin: string;
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
