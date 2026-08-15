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
  priority_fee_multiplier: number;
  max_priority_fee_gwei_cap: number;
  gas_limit_headroom_pct: number;
  jitter_ms_min: number;
  jitter_ms_max: number;
  gas_jitter_pct: number;
  wallets: WalletCfg[];
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
  | { type: 'snapshot'; armed: boolean; wallets: WalletStatus[] };
