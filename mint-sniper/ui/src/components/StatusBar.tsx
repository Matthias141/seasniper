import styles from './StatusBar.module.css';
import { useEffect, useState } from 'react';

/**
 * step 8b: the active target should always be visible, not buried behind
 * an "open config" click — it's the single fact that determines where the
 * next mint's money goes, so it stays in the persistent header alongside
 * ARMED/LINK, not just inside ConfigPanel or the target resolver.
 */
export function StatusBar({
  armed,
  connected,
  activeTarget,
}: {
  armed: boolean;
  connected: boolean;
  activeTarget?: string;
}) {
  const [time, setTime] = useState(new Date());
  useEffect(() => {
    const id = setInterval(() => setTime(new Date()), 1000);
    return () => clearInterval(id);
  }, []);

  return (
    <header className={styles.bar}>
      <div className={styles.brand}>
        <span className={styles.brandMark}>▲</span>
        <span className={styles.brandText}>MINT&nbsp;SNIPER</span>
        <span className={styles.brandSub}>CONTROL&nbsp;DECK</span>
      </div>

      <div className={styles.right}>
        <div className={styles.pill} title={activeTarget ? `Active target: ${activeTarget}` : 'No target set'}>
          <span className={styles.dot} />
          {activeTarget ? `TGT ${activeTarget.slice(0, 6)}…${activeTarget.slice(-4)}` : 'NO TARGET'}
        </div>
        <div className={`${styles.pill} ${connected ? styles.pillOk : styles.pillDown}`}>
          <span className={styles.dot} />
          {connected ? 'LINK OK' : 'NO LINK'}
        </div>
        <div className={`${styles.pill} ${armed ? styles.pillArmed : styles.pillIdle}`}>
          <span className={styles.dot} />
          {armed ? 'ARMED' : 'STANDBY'}
        </div>
        <time className={styles.clock}>
          {time.toLocaleTimeString('en-GB', { hour12: false })}
        </time>
      </div>
    </header>
  );
}
