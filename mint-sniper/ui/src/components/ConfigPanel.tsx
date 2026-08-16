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
              <span>Mint mode</span>
              <select value={draft.mint_mode} onChange={(e) => update('mint_mode', e.target.value as Config['mint_mode'])}>
                <option value="custom">custom</option>
                <option value="seadrop">seadrop</option>
              </select>
            </label>

            {draft.mint_mode === 'custom' && (
              <>
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
              </>
            )}

            {draft.mint_mode === 'seadrop' && (
              <>
                <label className={styles.field}>
                  <span>SeaDrop singleton address (blank = mainnet/Polygon default)</span>
                  <input
                    value={draft.seadrop_address}
                    onChange={(e) => update('seadrop_address', e.target.value)}
                    placeholder="0x00005EA00Ac477B1030CE78506496e8C2dE24bf5"
                    spellCheck={false}
                  />
                </label>

                <label className={styles.field}>
                  <span>Active nft_contract (use the Target Resolver panel to change this safely)</span>
                  <input value={draft.nft_contract} onChange={(e) => update('nft_contract', e.target.value)} spellCheck={false} />
                </label>

                <label className={styles.field}>
                  <span>Fee recipient</span>
                  <input value={draft.fee_recipient} onChange={(e) => update('fee_recipient', e.target.value)} spellCheck={false} />
                </label>

                <label className={styles.field}>
                  <span>Quantity per wallet</span>
                  <input
                    type="number"
                    min={1}
                    value={draft.quantity_per_wallet}
                    onChange={(e) => update('quantity_per_wallet', Number(e.target.value))}
                  />
                </label>
              </>
            )}

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

            <div className={styles.field}>
              <span>Copymint — tracked wallets</span>
              {draft.tracked_wallets.map((addr, i) => (
                <div className={styles.listRow} key={i}>
                  <input
                    value={addr}
                    onChange={(e) => {
                      const next = [...draft.tracked_wallets];
                      next[i] = e.target.value;
                      update('tracked_wallets', next);
                    }}
                    placeholder="0x..."
                    spellCheck={false}
                  />
                  <button
                    type="button"
                    className={styles.removeBtn}
                    onClick={() =>
                      update(
                        'tracked_wallets',
                        draft.tracked_wallets.filter((_, idx) => idx !== i),
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
                onClick={() => update('tracked_wallets', [...draft.tracked_wallets, ''])}
              >
                + add tracked wallet
              </button>
            </div>

            <div className={styles.gasGrid}>
              <label className={styles.field}>
                <span>Auto-fire FREE copymints</span>
                <input
                  type="checkbox"
                  checked={draft.copymint_auto_fire_free}
                  onChange={(e) => update('copymint_auto_fire_free', e.target.checked)}
                />
              </label>
              <label className={styles.field}>
                <span>Enable manual fire for PAID copymints</span>
                <input
                  type="checkbox"
                  checked={draft.copymint_auto_fire_paid}
                  onChange={(e) => update('copymint_auto_fire_paid', e.target.checked)}
                />
              </label>
              <label className={styles.field}>
                <span>Max paid copymint value (wei)</span>
                <input
                  type="number"
                  value={draft.max_copymint_price_wei}
                  onChange={(e) => update('max_copymint_price_wei', Number(e.target.value))}
                />
              </label>
            </div>
            <p className={styles.hint}>
              Paid copymints never auto-fire, regardless of these settings — they always require
              a manual click, and only up to the wei ceiling above.
            </p>

            <label className={styles.field}>
              <span>OpenSea API key env var name (step 8b/8c — never a raw key here)</span>
              <input
                value={draft.opensea_api_key_env}
                onChange={(e) => update('opensea_api_key_env', e.target.value)}
                placeholder="OPENSEA_API_KEY"
                spellCheck={false}
              />
            </label>
            <p className={styles.hint}>
              Needed to resolve an OpenSea collection URL or run a name search. Not needed for a
              raw contract address. Set the actual key as an env var on the host — same
              never-in-config-toml pattern as wallet private keys.
            </p>
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
