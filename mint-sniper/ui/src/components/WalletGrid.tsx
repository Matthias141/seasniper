import styles from './WalletGrid.module.css';
import type { WalletStatus } from '../types';

function truncate(addr: string) {
  return `${addr.slice(0, 6)}…${addr.slice(-4)}`;
}

export function WalletGrid({ wallets }: { wallets: WalletStatus[] }) {
  return (
    <section className={styles.panel}>
      <h2 className={styles.heading}>
        WALLETS <span className={styles.count}>{wallets.length.toString().padStart(2, '0')}</span>
      </h2>

      {wallets.length === 0 && <p className={styles.empty}>no wallets loaded — check config</p>}

      <div className={styles.grid}>
        {wallets.map((w) => (
          <div key={w.address} className={styles.card}>
            <div className={styles.row}>
              <span className={`${styles.healthDot} ${w.healthy ? styles.healthy : styles.unhealthy}`} />
              <span
                className={styles.address}
                title={w.address}
                onClick={() => navigator.clipboard?.writeText(w.address)}
              >
                {truncate(w.address)}
              </span>
            </div>
            <div className={styles.metrics}>
              <div>
                <span className={styles.metricLabel}>BAL</span>
                <span className={styles.metricValue}>{Number(w.balance_eth).toFixed(4)}</span>
              </div>
              <div>
                <span className={styles.metricLabel}>NONCE</span>
                <span className={styles.metricValue}>{w.nonce}</span>
              </div>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
