import { useEffect, useRef } from 'react';
import styles from './EventFeed.module.css';

export interface FeedLine {
  id: number;
  level: 'info' | 'warn' | 'error';
  message: string;
  ts: number;
}

export function EventFeed({ lines }: { lines: FeedLine[] }) {
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: 'end' });
  }, [lines.length]);

  return (
    <section className={styles.panel}>
      <h2 className={styles.heading}>EVENT LOG</h2>
      <div className={styles.feed}>
        {lines.length === 0 && <div className={styles.placeholder}>awaiting events…</div>}
        {lines.map((l) => (
          <div key={l.id} className={`${styles.line} ${styles[l.level]}`}>
            <span className={styles.ts}>
              {new Date(l.ts * 1000).toLocaleTimeString('en-GB', { hour12: false })}
            </span>
            <span className={styles.level}>{l.level.toUpperCase()}</span>
            <span className={styles.message}>{l.message}</span>
          </div>
        ))}
        <div ref={bottomRef} />
      </div>
    </section>
  );
}
