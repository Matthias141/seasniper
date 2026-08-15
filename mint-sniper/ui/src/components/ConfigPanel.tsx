import { useState } from 'react';
import styles from './ConfigPanel.module.css';
import type { Config } from '../types';
import { api } from '../lib/api';

export function ConfigPanel({
  config,
  onSaved,
  disabled,
}: {
  config: Config;
  onSaved: (cfg: Config) => void;
  disabled: boolean;
}) {
  const [draft, setDraft] = useState<Config>(config);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState(false);

  function update<K extends keyof Config>(key: K, value: Config[K]) {
    setDraft((d) => ({ ...d, [key]: value }));
  }

  function updateRpcUrl(i: number, value: string) {
    const next = [...draft.http_rpc_urls];
    next[i] = value;
    update('http_rpc_urls', next);
  }

  async function save() {
    setSaving(true);
    setError(null);
    try {
      await api.putConfig(draft);
      onSaved(draft);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'save failed');
    } finally {
      setSaving(false);
    }
  }

  return (
    <section className={styles.panel}>
      <button className={styles.toggle} onClick={() => setOpen((o) => !o)}>
        <span>CONFIG</span>
        <span className={styles.chevron}>{open ? '▾' : '▸'}</span>
      </button>

      {open && (
        <div className={styles.body}>
          <fieldset disabled={disabled || saving} className={styles.fieldset}>
            <label className={styles.field}>
              <span>Contract address</span>
              <input
                value={draft.contract_address}
                onChange={(e) => update('contract_address', e.target.value)}
                spellCheck={false}
              />
            </label>

            <label className={styles.field}>
              <span>Mint fn signature</span>
              <input
                value={draft.mint_fn_signature}
                onChange={(e) => update('mint_fn_signature', e.target.value)}
                spellCheck={false}
              />
            </label>

            <label className={styles.field}>
              <span>Mint-state fn signature</span>
              <input
                value={draft.mint_state_fn_signature}
                onChange={(e) => update('mint_state_fn_signature', e.target.value)}
                spellCheck={false}
              />
            </label>

            <label className={styles.field}>
              <span>Trigger mode</span>
              <select
                value={draft.trigger_mode}
                onChange={(e) => update('trigger_mode', e.target.value as Config['trigger_mode'])}
              >
                <option value="poll_state">poll_state</option>
                <option value="timestamp">timestamp</option>
                <option value="mempool_watch">mempool_watch</option>
              </select>
            </label>

            {draft.trigger_mode === 'timestamp' && (
              <label className={styles.field}>
                <span>Trigger unix timestamp</span>
                <input
                  type="number"
                  value={draft.trigger_timestamp_unix}
                  onChange={(e) => update('trigger_timestamp_unix', Number(e.target.value))}
                />
              </label>
            )}

            {draft.trigger_mode === 'mempool_watch' && (
              <label className={styles.field}>
                <span>Admin address (fires on pending tx from this address)</span>
                <input
                  value={draft.mint_enable_admin}
                  onChange={(e) => update('mint_enable_admin', e.target.value)}
                  placeholder="0x..."
                  spellCheck={false}
                />
              </label>
            )}

            <div className={styles.field}>
              <span>HTTP RPC endpoints (raced on broadcast)</span>
              {draft.http_rpc_urls.map((url, i) => (
                <div className={styles.listRow} key={i}>
                  <input value={url} onChange={(e) => updateRpcUrl(i, e.target.value)} spellCheck={false} />
                  <button
                    type="button"
                    className={styles.removeBtn}
                    onClick={() =>
                      update(
                        'http_rpc_urls',
                        draft.http_rpc_urls.filter((_, idx) => idx !== i),
                      )
                    }
                  >
                    ✕
                  </button>
                </div>
              ))}
              <button
                type="button"
                className={styles.addBtn}
                onClick={() => update('http_rpc_urls', [...draft.http_rpc_urls, ''])}
              >
                + add endpoint
              </button>
            </div>

            <div className={styles.gasGrid}>
              <label className={styles.field}>
                <span>Priority fee ×</span>
                <input
                  type="number"
                  step="0.1"
                  value={draft.priority_fee_multiplier}
                  onChange={(e) => update('priority_fee_multiplier', Number(e.target.value))}
                />
              </label>
              <label className={styles.field}>
                <span>Max priority fee (gwei)</span>
                <input
                  type="number"
                  step="0.5"
                  value={draft.max_priority_fee_gwei_cap}
                  onChange={(e) => update('max_priority_fee_gwei_cap', Number(e.target.value))}
                />
              </label>
              <label className={styles.field}>
                <span>Gas jitter %</span>
                <input
                  type="number"
                  value={draft.gas_jitter_pct}
                  onChange={(e) => update('gas_jitter_pct', Number(e.target.value))}
                />
              </label>
              <label className={styles.field}>
                <span>Timing jitter (ms)</span>
                <div className={styles.listRow}>
                  <input
                    type="number"
                    value={draft.jitter_ms_min}
                    onChange={(e) => update('jitter_ms_min', Number(e.target.value))}
                  />
                  <input
                    type="number"
                    value={draft.jitter_ms_max}
                    onChange={(e) => update('jitter_ms_max', Number(e.target.value))}
                  />
                </div>
              </label>
            </div>

            <div className={styles.field}>
              <span>Wallets (env var names — never raw keys, set on the host)</span>
              {draft.wallets.map((w, i) => (
                <div className={styles.listRow} key={i}>
                  <input
                    value={w.private_key_env}
                    onChange={(e) => {
                      const next = [...draft.wallets];
                      next[i] = { private_key_env: e.target.value };
                      update('wallets', next);
                    }}
                    spellCheck={false}
                  />
                  <button
                    type="button"
                    className={styles.removeBtn}
                    onClick={() => update('wallets', draft.wallets.filter((_, idx) => idx !== i))}
                  >
                    ✕
                  </button>
                </div>
              ))}
              <button
                type="button"
                className={styles.addBtn}
                onClick={() =>
                  update('wallets', [
                    ...draft.wallets,
                    { private_key_env: `SNIPER_PK_${draft.wallets.length + 1}` },
                  ])
                }
              >
                + add wallet
              </button>
            </div>
          </fieldset>

          {disabled && <p className={styles.lockedNote}>disarm before editing config</p>}
          {error && <p className={styles.error}>{error}</p>}

          <button className={styles.save} onClick={save} disabled={disabled || saving}>
            {saving ? 'SAVING…' : 'SAVE + PERSIST TO DISK'}
          </button>
          <p className={styles.hint}>
            Changing wallets or contract requires a bot restart to re-derive signers.
          </p>
        </div>
      )}
    </section>
  );
}
