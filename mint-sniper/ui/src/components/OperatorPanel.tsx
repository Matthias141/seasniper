import { useEffect, useRef, useState } from 'react';
import styles from './OperatorPanel.module.css';
import { api } from '../lib/api';
import { DelegatedRunStatus, type DelegatedReceiverRow } from './DelegatedRunStatus';
import type { Config, DelegatedPreflightResult, DelegatedStatus } from '../types';

// Simple BigInt-based wei -> ETH string, avoids Number precision loss on
// large wei values — same approach as TargetResolver.tsx's own weiToEth.
function weiToEth(wei: string): string {
  try {
    const v = BigInt(wei);
    const whole = v / 1_000_000_000_000_000_000n;
    const frac = (v % 1_000_000_000_000_000_000n).toString().padStart(18, '0').replace(/0+$/, '');
    return frac ? `${whole}.${frac}` : whole.toString();
  } catch {
    return wei;
  }
}

// STEP 31a — the backend's operator_balance_eth comes straight from
// alloy's format_units(bal, "ether"), which can carry up to 18 decimal
// places with no truncation (e.g. "0.428431920000000000"). Rendered
// un-truncated in a nowrap flex item, this real value was wide enough
// to overflow a 390px viewport by ~2px — confirmed live via Playwright,
// not a hypothetical. String-truncated here (never parseFloat/toFixed,
// same "no float precision loss on a real balance" reasoning as
// weiToEth above) to a fixed number of decimals for display only; the
// full-precision value is still what's used anywhere non-display.
function truncateDecimalDisplay(decimalStr: string, maxDecimals = 6): string {
  if (decimalStr === '?') return decimalStr;
  const dot = decimalStr.indexOf('.');
  if (dot === -1 || decimalStr.length - dot - 1 <= maxDecimals) return decimalStr;
  return decimalStr.slice(0, dot + 1 + maxDecimals);
}

function truncateAddress(addr: string): string {
  return addr.length > 12 ? `${addr.slice(0, 6)}…${addr.slice(-4)}` : addr;
}

export interface DelegatedRunState {
  delegateCount: number;
  estimatedMaxSpendWei: string;
  results: Map<number, { address: string; success: boolean; detail: string }>;
  complete: { minted: number; attempted: number; totalCostWei: string } | null;
}

/**
 * Delegated mint mode (v1, DELEGATED_SERIAL) control panel — MintDash-
 * style operator/receiver wallet mint, NOT a batch/single-transaction
 * mint (every label here says DELEGATED_SERIAL for that exact reason,
 * see src/delegated/mod.rs's doc comment). Only rendered when
 * config.mint_execution === "delegated" (App.tsx gates this).
 *
 * Arm-flow reuses TriggerConsole.tsx's own "lift safety cover" two-step
 * pattern (confirmed decision, not a new modal or a plain toggle) — the
 * pre-arm summary block IS the cover: it must be visibly acknowledged
 * before FIRE becomes clickable, and auto-recloses after 5s unattended,
 * same as TriggerConsole's own cover.
 *
 * ABSOLUTE RULE, enforced by construction, not just convention: no
 * mnemonic, private key, or seed material is ever fetched, held in
 * state, passed as a prop, or rendered by this component or anything it
 * renders — every value here is either a public address (from
 * GET /api/delegated/status, which itself can only ever return
 * addresses — see api.rs's delegated_secrets_tests) or a plain number/
 * wei string. See ui/src/components/OperatorPanel.test.tsx for the test
 * asserting this holds for every prop/state/DOM node this component can
 * reach.
 */
export function OperatorPanel({
  config,
  run,
  onRunReset,
}: {
  config: Config;
  run: DelegatedRunState | null;
  onRunReset: () => void;
}) {
  const [status, setStatus] = useState<DelegatedStatus | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [preflight, setPreflight] = useState<DelegatedPreflightResult | null>(null);
  const [preflightBusy, setPreflightBusy] = useState(false);
  const [preflightError, setPreflightError] = useState<string | null>(null);
  const [coverLifted, setCoverLifted] = useState(false);
  const [firing, setFiring] = useState(false);
  const [fireError, setFireError] = useState<string | null>(null);
  const autoCloseRef = useRef<ReturnType<typeof setTimeout>>();

  useEffect(() => {
    api
      .getDelegatedStatus()
      .then(setStatus)
      .catch((e) => setStatusError(e instanceof Error ? e.message : 'failed to load operator status'));
  }, []);

  // Same auto-recloses-after-5s-unattended safety property as
  // TriggerConsole.tsx's own cover — a lifted cover left unattended is
  // exactly the failure mode a physical launch console's cover prevents.
  useEffect(() => {
    if (coverLifted) {
      autoCloseRef.current = setTimeout(() => setCoverLifted(false), 5000);
    }
    return () => clearTimeout(autoCloseRef.current);
  }, [coverLifted]);

  async function runPreflight() {
    setPreflightBusy(true);
    setPreflightError(null);
    setCoverLifted(false);
    try {
      const result = await api.preflightDelegated();
      setPreflight(result);
    } catch (e) {
      setPreflightError(e instanceof Error ? e.message : 'preflight failed');
    } finally {
      setPreflightBusy(false);
    }
  }

  async function fire() {
    setCoverLifted(false);
    setFiring(true);
    setFireError(null);
    onRunReset();
    try {
      await api.fireDelegated();
    } catch (e) {
      setFireError(e instanceof Error ? e.message : 'fire failed');
    } finally {
      setFiring(false);
    }
  }

  if (statusError) {
    return (
      <section className={styles.panel}>
        <h2 className={styles.heading}>OPERATOR PANEL — DELEGATED_SERIAL</h2>
        <div className={styles.errorBox}>{statusError}</div>
      </section>
    );
  }

  if (!status) {
    return (
      <section className={styles.panel}>
        <h2 className={styles.heading}>OPERATOR PANEL — DELEGATED_SERIAL</h2>
        <div className={styles.placeholder}>loading…</div>
      </section>
    );
  }

  const rows: DelegatedReceiverRow[] = status.receivers.map((r) => {
    const result = run?.results.get(r.index);
    return {
      index: r.index,
      address: r.address,
      status: !result ? 'queued' : result.success ? 'confirmed' : 'failed',
      detail: result?.detail ?? null,
    };
  });

  return (
    <section className={styles.panel}>
      <h2 className={styles.heading}>OPERATOR PANEL — DELEGATED_SERIAL</h2>

      <div className={styles.header}>
        <div>
          <div className={styles.label}>OPERATOR (only funded wallet)</div>
          <div className={styles.address}>{status.operator_address}</div>
        </div>
        <div className={styles.balance} title={`${status.operator_balance_eth} ETH`}>
          {truncateDecimalDisplay(status.operator_balance_eth)} ETH
        </div>
      </div>

      <div className={styles.capacityLine}>
        {status.delegate_count} / {status.max_delegates} receivers active
      </div>

      <div className={styles.receiverTable}>
        {status.receivers.map((r) => (
          <div key={r.index} className={styles.receiverRow}>
            <span className={styles.receiverIndex}>#{r.index}</span>
            <span className={styles.receiverAddress}>{truncateAddress(r.address)}</span>
            <button
              className={styles.copyBtn}
              onClick={() => navigator.clipboard?.writeText(r.address)}
              title="copy address"
            >
              copy
            </button>
            {/* Unfunded by design — this is reassurance, not an error
                state. Receivers never hold gas; only the operator pays. */}
            <span className={styles.receiverBalance}>~0 ETH (expected — unfunded by design)</span>
          </div>
        ))}
      </div>

      {!run && (
        <div className={styles.preflightBlock}>
          <button className={styles.preflightBtn} onClick={runPreflight} disabled={preflightBusy}>
            {preflightBusy ? 'RUNNING PREFLIGHT…' : 'RUN PREFLIGHT'}
          </button>
          {preflightError && <div className={styles.errorBox}>{preflightError}</div>}

          {preflight?.outcome === 'minter_mismatch' && (
            <div className={styles.errorBox}>
              <strong>REFUSED TO ARM — minterIfNotPayer rejected by contract.</strong>
              <div>This contract does not support delegated minting. Falling back to parallel mode is never done automatically.</div>
              <pre className={styles.rawReason}>{preflight.revert_reason}</pre>
            </div>
          )}
          {preflight?.outcome === 'other_failure' && (
            <div className={styles.errorBox}>
              <strong>Preflight failed (unrelated to minterIfNotPayer).</strong>
              <pre className={styles.rawReason}>{preflight.revert_reason}</pre>
            </div>
          )}

          {preflight?.outcome === 'ok' && (
            <div className={styles.summaryBlock}>
              <div className={styles.summaryTitle}>PRE-ARM SUMMARY</div>
              <dl className={styles.summaryGrid}>
                <dt>Contract</dt>
                <dd>{config.nft_contract || '—'}</dd>
                <dt>Quantity per wallet</dt>
                <dd>{config.quantity_per_wallet}</dd>
                <dt>Receivers (delegate_count)</dt>
                <dd>{preflight.delegate_count}</dd>
                <dt>Estimated max spend</dt>
                <dd>{weiToEth(preflight.estimated_max_spend_wei)} ETH</dd>
                <dt>Mode</dt>
                <dd className={styles.modeLabel}>DELEGATED_SERIAL</dd>
              </dl>

              <div className={`${styles.cover} ${coverLifted ? styles.coverLifted : ''}`}>
                <button className={styles.coverBtn} onClick={() => setCoverLifted(true)} disabled={coverLifted || firing}>
                  🔒 LIFT SAFETY COVER TO ACKNOWLEDGE SPEND
                </button>
              </div>
              <button className={`${styles.fireBtn} ${coverLifted ? styles.fireBtnLive : ''}`} onClick={fire} disabled={!coverLifted || firing}>
                {firing ? 'STARTING…' : 'FIRE DELEGATED_SERIAL'}
              </button>
              {fireError && <div className={styles.errorBox}>{fireError}</div>}
            </div>
          )}
        </div>
      )}

      {run && (
        <>
          <DelegatedRunStatus rows={rows} total={run.delegateCount} />
          {run.complete && (
            <pre className={styles.terminalCard}>
{`✅ Delegated mint complete
Collection: ${config.nft_contract || '—'}
Minted: ${run.complete.minted} of ${run.complete.attempted} receivers
Total cost: ${weiToEth(run.complete.totalCostWei)} ETH
Mode: DELEGATED_SERIAL`}
            </pre>
          )}
          <button className={styles.resetBtn} onClick={onRunReset} disabled={!run.complete}>
            {run.complete ? 'START ANOTHER RUN' : 'RUN IN PROGRESS…'}
          </button>
        </>
      )}
    </section>
  );
}
