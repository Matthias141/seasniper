import { useEffect, useRef, useState } from 'react';
import styles from './TriggerConsole.module.css';
import { api } from '../lib/api';

export function TriggerConsole({ armed }: { armed: boolean }) {
  const [coverLifted, setCoverLifted] = useState(false);
  const [busy, setBusy] = useState(false);
  const autoCloseRef = useRef<ReturnType<typeof setTimeout>>();

  // Safety cover auto-recloses if FIRE isn't pressed within 5s of lifting —
  // a lifted cover left unattended is exactly the failure mode a physical
  // launch console's cover prevents: an accidental brush of the button.
  useEffect(() => {
    if (coverLifted) {
      autoCloseRef.current = setTimeout(() => setCoverLifted(false), 5000);
    }
    return () => clearTimeout(autoCloseRef.current);
  }, [coverLifted]);

  async function handleArmToggle() {
    setBusy(true);
    try {
      if (armed) await api.abort();
      else await api.arm();
    } finally {
      setBusy(false);
    }
  }

  async function handleFire() {
    setCoverLifted(false);
    setBusy(true);
    try {
      await api.fireNow();
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className={styles.console}>
      <div className={styles.armRow}>
        <div className={styles.armLabel}>
          <span>WATCHER</span>
          <span className={styles.armSub}>auto-fires on trigger condition</span>
        </div>
        <button
          className={`${styles.armSwitch} ${armed ? styles.armSwitchOn : ''}`}
          onClick={handleArmToggle}
          disabled={busy}
          aria-pressed={armed}
        >
          <span className={styles.armKnob} />
        </button>
      </div>

      <div className={styles.fireBlock}>
        <div className={`${styles.cover} ${coverLifted ? styles.coverLifted : ''}`}>
          <button
            className={styles.coverBtn}
            onClick={() => setCoverLifted(true)}
            disabled={coverLifted || busy}
          >
            <span className={styles.coverIcon}>🔒</span>
            LIFT SAFETY COVER
          </button>
        </div>

        <button
          className={`${styles.fireBtn} ${coverLifted ? styles.fireBtnLive : ''}`}
          onClick={handleFire}
          disabled={!coverLifted || busy}
        >
          FIRE NOW
        </button>
      </div>

      <p className={styles.note}>
        Fires all wallets immediately, regardless of watcher state. Use when you don't trust
        the auto-trigger and are watching the drop yourself.
      </p>
    </section>
  );
}
