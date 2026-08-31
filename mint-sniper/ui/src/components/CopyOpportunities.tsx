import { useState } from 'react';
import styles from './CopyOpportunities.module.css';
import { api } from '../lib/api';
import type { CopymintEligibilityReport, CopymintSkipReason } from '../types';

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

// step 31b — a skip that never became (or, for exceeds_price_ceiling,
// became AND ALSO gets this) a card of its own — see copymint.rs's
// CopymintSkipReason doc comment for why exceeds_price_ceiling is
// deliberately not exclusive with the opportunity card above it.
export interface CopymintSkip {
  trackedWallet: string;
  nftContract: string;
  reason: CopymintSkipReason;
  actionableHint: string | null;
}

// step 31b — human-readable label + badge tone per structured reason
// code, kept in one place so a new CopymintSkipReason variant can't
// silently render as blank text (exhaustive switch, TS enforces it).
function skipLabel(reason: CopymintSkipReason): string {
  switch (reason.code) {
    case 'drop_lookup_failed':
      return 'LOOKUP FAILED';
    case 'not_currently_live':
      return 'NOT LIVE';
    case 'exceeds_price_ceiling':
      return 'OVER CEILING';
    case 'calldata_encoding_failed':
      return 'ENCODING FAILED';
  }
}

function skipDetail(reason: CopymintSkipReason): string {
  switch (reason.code) {
    case 'drop_lookup_failed':
      return reason.detail;
    case 'not_currently_live':
      return `start ${reason.start_time}, end ${reason.end_time}, now ${reason.now}`;
    case 'exceeds_price_ceiling':
      return `${reason.total_value_wei} wei > ceiling ${reason.ceiling_wei} wei`;
    case 'calldata_encoding_failed':
      return reason.detail;
  }
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
 *
 * STEP 31b additions, both copymint-specific and with zero bearing on
 * the parallel-EOA hot fire path (neither of these calls anything
 * `executor.rs`/`fire_prepared` touches):
 * - Structured `skips` list — a candidate that never became an
 *   opportunity card at all (or, for the over-ceiling case, one that did
 *   AND gets this too) now renders as its own real card with a named
 *   reason code and, where one exists, a concrete actionable next step —
 *   not just a `bus::log` line an operator has to go find and parse.
 * - A "CHECK ELIGIBILITY" button per opportunity — calls
 *   `POST /api/copymint/eligibility` (read-only, re-reads getPublicDrop +
 *   getMintStats live, never cached) and shows a precomputed
 *   "N/M wallets eligible" ratio before the operator commits to firing.
 */
export function CopyOpportunities({
  opportunities,
  skips,
  allowManualFire,
}: {
  opportunities: CopyOpportunity[];
  skips: CopymintSkip[];
  allowManualFire: boolean;
}) {
  const [firing, setFiring] = useState<Record<string, 'pending' | 'fired' | 'error'>>({});
  const [errorMsg, setErrorMsg] = useState<Record<string, string>>({});
  const [eligibility, setEligibility] = useState<Record<string, CopymintEligibilityReport | 'pending' | 'error'>>({});

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

  async function checkEligibility(nftContract: string) {
    setEligibility((e) => ({ ...e, [nftContract]: 'pending' }));
    try {
      const report = await api.checkCopymintEligibility(nftContract);
      setEligibility((e) => ({ ...e, [nftContract]: report }));
    } catch {
      setEligibility((e) => ({ ...e, [nftContract]: 'error' }));
    }
  }

  return (
    <section className={styles.panel}>
      <h2 className={styles.heading}>COPYMINT OPPORTUNITIES</h2>
      <div className={styles.list}>
        {opportunities.length === 0 && skips.length === 0 && (
          <div className={styles.placeholder}>none detected yet…</div>
        )}
        {opportunities.map((o) => {
          const status = firing[o.nftContract];
          const overCeiling = !o.isFree && !o.fireable;
          const cardClass = o.isFree ? styles.free : overCeiling ? styles.paidOverCeiling : styles.paid;
          const label = o.isFree ? 'FREE — AUTO' : overCeiling ? 'PAID — OVER CEILING' : 'PAID — MANUAL';
          const elig = eligibility[o.nftContract];

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
                {elig && elig !== 'pending' && elig !== 'error' && (
                  <div className={styles.eligibilityLine}>
                    {elig.eligible_count}/{elig.total_count} wallets eligible · {elig.remaining_supply} of{' '}
                    {elig.max_supply} remaining supply
                  </div>
                )}
                {elig === 'error' && <div className={styles.eligibilityLine}>eligibility check failed</div>}
              </div>

              <div className={styles.cardActions}>
                <button
                  className={styles.eligibilityBtn}
                  onClick={() => checkEligibility(o.nftContract)}
                  disabled={elig === 'pending'}
                >
                  {elig === 'pending' ? 'CHECKING…' : 'CHECK ELIGIBILITY'}
                </button>
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
            </div>
          );
        })}
        {skips.map((s, i) => (
          <div key={`${s.nftContract}-${i}`} className={`${styles.card} ${styles.skipped}`}>
            <div>
              <span className={styles.badge}>{skipLabel(s.reason)}</span>
              <div className={styles.contract}>{s.nftContract}</div>
              <div className={styles.meta}>
                via {s.trackedWallet.slice(0, 10)}… · {skipDetail(s.reason)}
              </div>
              {s.actionableHint && <div className={styles.hint}>→ {s.actionableHint}</div>}
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
