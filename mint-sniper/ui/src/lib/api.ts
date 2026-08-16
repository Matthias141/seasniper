import type { Config, StatusResponse } from '../types';

// Bootstrapped once at app startup from GET /api/token — the one route the
// Rust backend doesn't require the token on, since this is how the token
// reaches the UI in the first place. Never hardcoded, never fetched from
// anywhere but this same backend. See ui/README.md's security note for
// what this token does and doesn't protect against.
let authToken: string | null = null;

export async function initAuth(): Promise<void> {
  const res = await fetch('/api/token');
  if (!res.ok) throw new Error(`failed to fetch API token: ${res.status}`);
  const data = (await res.json()) as { token: string };
  authToken = data.token;
}

export function getAuthToken(): string | null {
  return authToken;
}

function authHeaders(extra?: Record<string, string>): Record<string, string> {
  return {
    ...extra,
    ...(authToken ? { Authorization: `Bearer ${authToken}` } : {}),
  };
}

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}: ${await res.text().catch(() => '')}`);
  }
  return res.json() as Promise<T>;
}

export const api = {
  getConfig: () => fetch('/api/config', { headers: authHeaders() }).then((r) => json<Config>(r)),

  putConfig: (cfg: Config) =>
    fetch('/api/config', {
      method: 'PUT',
      headers: authHeaders({ 'Content-Type': 'application/json' }),
      body: JSON.stringify(cfg),
    }).then((r) => {
      if (!r.ok) throw new Error(`save failed: ${r.status}`);
    }),

  getStatus: () => fetch('/api/status', { headers: authHeaders() }).then((r) => json<StatusResponse>(r)),

  arm: () => fetch('/api/arm', { method: 'POST', headers: authHeaders() }),
  abort: () => fetch('/api/abort', { method: 'POST', headers: authHeaders() }),
  fireNow: () => fetch('/api/trigger', { method: 'POST', headers: authHeaders() }),

  // Server independently re-verifies liveness + max_copymint_price_wei
  // fresh via getPublicDrop before firing — never trusts the price this
  // client is currently displaying. See api.rs's post_copymint_fire.
  // Resolves with the server's plain-text confirmation/rejection message.
  fireCopymint: async (nftContract: string, feeRecipient: string): Promise<string> => {
    const res = await fetch('/api/copymint/fire', {
      method: 'POST',
      headers: authHeaders({ 'Content-Type': 'application/json' }),
      body: JSON.stringify({ nft_contract: nftContract, fee_recipient: feeRecipient }),
    });
    const text = await res.text();
    if (!res.ok) throw new Error(text || `${res.status} ${res.statusText}`);
    return text;
  },
};
