import { useCallback, useEffect, useState } from 'react';
import styles from './App.module.css';
import { StatusBar } from './components/StatusBar';
import { WalletGrid } from './components/WalletGrid';
import { EventFeed, type FeedLine } from './components/EventFeed';
import { ConfigPanel } from './components/ConfigPanel';
import { TriggerConsole } from './components/TriggerConsole';
import { useEventSocket } from './lib/useEventSocket';
import { api } from './lib/api';
import type { Config, ServerEvent, WalletStatus } from './types';

let lineId = 0;

export default function App() {
  const [config, setConfig] = useState<Config | null>(null);
  const [wallets, setWallets] = useState<WalletStatus[]>([]);
  const [armed, setArmed] = useState(false);
  const [lines, setLines] = useState<FeedLine[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);

  const pushLine = useCallback((level: FeedLine['level'], message: string, ts?: number) => {
    setLines((prev) => [
      ...prev.slice(-199), // cap feed length — this is a control panel, not an archive
      { id: lineId++, level, message, ts: ts ?? Date.now() / 1000 },
    ]);
  }, []);

  const handleEvent = useCallback(
    (event: ServerEvent) => {
      switch (event.type) {
        case 'snapshot':
          setArmed(event.armed);
          setWallets(event.wallets);
          break;
        case 'log':
          pushLine(event.level, event.message, event.ts);
          break;
        case 'armed_state':
          setArmed(event.armed);
          break;
        case 'wallet_update':
          setWallets((prev) =>
            prev.map((w) =>
              w.address === event.address
                ? { ...w, balance_eth: event.balance_eth, healthy: event.healthy }
                : w,
            ),
          );
          break;
        case 'mint_result':
          pushLine(
            event.success ? 'info' : 'error',
            `${event.address}: ${event.success ? 'confirmed' : 'failed'} — ${event.detail}`,
          );
          break;
        case 'trigger_fired':
          pushLine('warn', `trigger fired (${event.manual ? 'manual' : 'auto'})`);
          break;
        case 'rpc_health':
          if (!event.healthy) pushLine('warn', `RPC degraded: ${event.url} (${event.latency_ms}ms)`);
          break;
      }
    },
    [pushLine],
  );

  const { connected } = useEventSocket(handleEvent);

  useEffect(() => {
    api
      .getConfig()
      .then(setConfig)
      .catch((e) => setLoadError(e.message));
    api
      .getStatus()
      .then((s) => {
        setArmed(s.armed);
        setWallets(s.wallets);
      })
      .catch(() => {});
  }, []);

  return (
    <div className={styles.shell}>
      <StatusBar armed={armed} connected={connected} />

      {loadError && <div className={styles.banner}>config load failed: {loadError}</div>}

      <main className={styles.grid}>
        <div className={styles.left}>
          <WalletGrid wallets={wallets} />
          <EventFeed lines={lines} />
        </div>

        <div className={styles.right}>
          <TriggerConsole armed={armed} />
          {config && (
            <ConfigPanel config={config} onSaved={setConfig} disabled={armed} />
          )}
        </div>
      </main>
    </div>
  );
}
