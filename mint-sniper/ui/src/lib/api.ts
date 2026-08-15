import type { Config, StatusResponse } from '../types';

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}: ${await res.text().catch(() => '')}`);
  }
  return res.json() as Promise<T>;
}

export const api = {
  getConfig: () => fetch('/api/config').then((r) => json<Config>(r)),

  putConfig: (cfg: Config) =>
    fetch('/api/config', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(cfg),
    }).then((r) => {
      if (!r.ok) throw new Error(`save failed: ${r.status}`);
    }),

  getStatus: () => fetch('/api/status').then((r) => json<StatusResponse>(r)),

  arm: () => fetch('/api/arm', { method: 'POST' }),
  abort: () => fetch('/api/abort', { method: 'POST' }),
  fireNow: () => fetch('/api/trigger', { method: 'POST' }),
};
