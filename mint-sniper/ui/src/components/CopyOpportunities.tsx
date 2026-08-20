import { useState } from 'react';
import styles from './CopyOpportunities.module.css';
import { api } from '../lib/api';

export interface CopyOpportunity {
  trackedWallet: string;
  nftContract: string;
  feeRecipient: string;
  mintPriceWei: string;
  totalValueWei: string;
  quantity: number;
  isFree: boolean;
  fireable: boolean;
}

/**
 * step 6d: surfaces every copymint opportunity the backend has detected
 * and independently verified (see copymint.rs's doc comment — a fresh
 * getPublicDrop check stands in for the human review every other trigger
 * mode gets from being manually configured). Free vs paid is a visual
 * distinction, not just a text field, on purpose: a free opportunity you
 * can trust the auto-fire default on should read completely differently
 * from a paid one waiting on your click.
 *
 * `allowManualFire` mirrors `copymint_auto_fire_paid` from config — per
 * that field's own doc comment, it ONLY controls whether this button is
 * shown/enabled, never whether the backend would accept the request. The
 * backend's /api/copymint/fire route doesn't read that flag at all; a
 * paid opportunity is only ever one click away because a human is
 * looking at this exact panel and chose to click, not because any config
 * value made it automatic.
 */
export function CopyOpportunities({
  opportunities,
  allowManualFire,
}: {
  opportunities: CopyOpportunity[];
  allowManualFire: boolean;
}) {
  const [firing, setFiring] = useState<Record<string, 'pending' | 'fired' | 'error'>>({});
  const [errorMsg, setErrorMsg] = useState<Record<string, string>>({});

  async function fire(o: CopyOpportunity) {
    setFiring((f) => ({ ...f, [o.nftContract]: 'pending' }));
    try {
      await api.fireCopymint(o.nftContract, o.feeRecipient);
      setFiring((f) => ({ ...f, [o.nftContract]: 'fired' }));
    } catch (e) {
      setFiring((f) => ({ ...f, [o.nftContract]: 'error' }));
      setErrorMsg((m) => ({ ...m, [o.nftContract]: e instanceof Error ? e.message : 'fire failed' }));
    }
  }

  return (
    <section className={styles.panel}>
      <h2 className={styles.heading}>COPYMINT OPPORTUNITIES</h2>
      <div className={styles.list}>
        {opportunities.length === 0 && (
          <div className={styles.placeholder}>none detected yet…</div>
        )}
        {opportunities.map((o) => {
          const status = firing[o.nftContract];
          const overCeiling = !o.isFree && !o.fireable;
          const cardClass = o.isFree ? styles.free : overCeiling ? styles.paidOverCeiling : styles.paid;
          const label = o.isFree ? 'FREE — AUTO' : overCeiling ? 'PAID — OVER CEILING' : 'PAID — MANUAL';

          return (
            <div key={o.nftContract} className={`${styles.card} ${cardClass}`}>
              <div>
                <span className={styles.badge}>{label}</span>
                <div className={styles.contract}>{o.nftContract}</div>
                <div className={styles.meta}>
                  via {o.trackedWallet.slice(0, 10)}… · qty {o.quantity} ·{' '}
                  {o.isFree ? 'free' : `${o.totalValueWei} wei`}
                </div>
                {status === 'error' && (
                  <div className={styles.meta}>{errorMsg[o.nftContract]}</div>
                )}
              </div>

              {!o.isFree && allowManualFire && (
                <button
                  className={`${styles.fireBtn} ${status === 'fired' ? styles.fired : ''}`}
                  disabled={!o.fireable || status === 'pending' || status === 'fired'}
                  onClick={() => fire(o)}
                >
                  {status === 'pending'
                    ? 'FIRING…'
                    : status === 'fired'
                      ? 'FIRED'
                      : status === 'error'
                        ? 'RETRY FIRE'
                        : 'FIRE'}
                </button>
              )}
            </div>
          );
        })}
      </div>
    </section>
  );
}
