import { useState } from 'react';
import styles from './DelegatedRunStatus.module.css';

export interface DelegatedReceiverRow {
  index: number;
  address: string;
  status: 'queued' | 'confirmed' | 'failed';
  detail: string | null; // tx hash on success, verbatim revert/error reason on failure
}

/**
 * Delegated mint mode (v1, DELEGATED_SERIAL) — per-receiver live status
 * list. Styled to match CopyOpportunities.tsx's existing card
 * conventions (same tokens, same badge/card shape) but built as its own
 * component with real expand/collapse, rather than extending
 * CopyOpportunities.tsx itself — these are two unrelated features
 * (copy-trading vs. delegated firing) that happen to share a visual
 * language, not one feature growing into the other. See
 * executor.rs/mod.rs's own doc comments for why every label here says
 * "confirmed"/"failed", never "batch" — this is N independent sequential
 * mintPublic calls, not one transaction.
 *
 * A failure's `detail` is rendered VERBATIM, never paraphrased or
 * re-summarized — see the original feature spec's explicit requirement
 * that a revert reason never be softened into a generic message.
 */
export function DelegatedRunStatus({ rows, total }: { rows: DelegatedReceiverRow[]; total: number }) {
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  const confirmedCount = rows.filter((r) => r.status === 'confirmed').length;
  const failedCount = rows.filter((r) => r.status === 'failed').length;
  const settledCount = confirmedCount + failedCount;

  function toggle(index: number) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  }

  return (
    <section className={styles.panel}>
      <div className={styles.headerRow}>
        <h2 className={styles.heading}>DELEGATED_SERIAL RUN</h2>
        <span className={styles.progress}>
          {settledCount}/{total} settled
        </span>
      </div>
      <div className={styles.list}>
        {rows.map((r) => {
          const cardClass =
            r.status === 'confirmed' ? styles.confirmed : r.status === 'failed' ? styles.failed : styles.queued;
          const label = r.status === 'confirmed' ? 'CONFIRMED' : r.status === 'failed' ? 'FAILED' : `QUEUED (${r.index}/${total})`;
          const isOpen = expanded.has(r.index);
          const canExpand = r.status === 'failed' && !!r.detail;

          return (
            <div key={r.index} className={`${styles.card} ${cardClass}`}>
              <div className={styles.row}>
                <div>
                  <span className={styles.badge}>{label}</span>
                  <div className={styles.address}>{r.address}</div>
                </div>
                {r.status === 'confirmed' && r.detail && (
                  <div className={styles.meta}>{r.detail}</div>
                )}
                {canExpand && (
                  <button className={styles.expandBtn} onClick={() => toggle(r.index)}>
                    {isOpen ? 'HIDE REASON' : 'WHY IT FAILED'}
                  </button>
                )}
              </div>
              {canExpand && isOpen && <pre className={styles.rawDetail}>{r.detail}</pre>}
            </div>
          );
        })}
      </div>
    </section>
  );
}
