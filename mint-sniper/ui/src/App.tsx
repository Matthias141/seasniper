import { useCallback, useEffect, useState } from 'react';
import styles from './App.module.css';
import { StatusBar } from './components/StatusBar';
import { WalletGrid } from './components/WalletGrid';
import { EventFeed, type FeedLine } from './components/EventFeed';
import { ConfigPanel } from './components/ConfigPanel';
import { TriggerConsole } from './components/TriggerConsole';
import { CopyOpportunities, type CopyOpportunity } from './components/CopyOpportunities';
import { TargetResolver } from './components/TargetResolver';
import { useEventSocket } from './lib/useEventSocket';
import { api, initAuth } from './lib/api';
import type { Config, ServerEvent, WalletStatus } from './types';

let lineId = 0;

// Gates the whole app behind GET /api/token resolving first — the WS hook
// and every api.ts call need the token already in memory before they fire,
// not racing it. A failed fetch here means the backend isn't reachable at
// all, which is worth a distinct message from "config load failed".
export default function App() {
  const [authReady, setAuthReady] = useState(false);
  const [authError, setAuthError] = useState<string | null>(null);

  useEffect(() => {
    initAuth()
      .then(() => setAuthReady(true))
      .catch((e) => setAuthError(e.message));
  }, []);

  if (authError) {
    return (
      <div className={styles.shell}>
        <div className={styles.banner}>couldn't reach the bot: {authError}</div>
      </div>
    );
  }

  if (!authReady) return null;

  return <Control />;
}

function Control() {
  const [config, setConfig] = useState<Config | null>(null);
  const [wallets, setWallets] = useState<WalletStatus[]>([]);
  const [armed, setArmed] = useState(false);
  const [lines, setLines] = useState<FeedLine[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  // Keyed by nft_contract — a repeat sighting of the same drop (multiple
  // tracked wallets, or the pending tx seen more than once) updates in
  // place rather than piling up duplicate cards.
  const [copyOpportunities, setCopyOpportunities] = useState<Map<string, CopyOpportunity>>(
    new Map(),
  );

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
        case 'copy_opportunity':
          setCopyOpportunities((prev) => {
            const next = new Map(prev);
            next.set(event.nft_contract, {
              trackedWallet: event.tracked_wallet,
              nftContract: event.nft_contract,
              feeRecipient: event.fee_recipient,
              mintPriceWei: event.mint_price_wei,
              totalValueWei: event.total_value_wei,
              quantity: event.quantity,
              isFree: event.is_free,
              fireable: event.fireable,
            });
            return next;
          });
          pushLine(
            'warn',
            `copymint: ${event.tracked_wallet} minting ${event.nft_contract} — ${
              event.is_free ? 'FREE' : `${event.total_value_wei} wei`
            }`,
          );
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
      <StatusBar armed={armed} connected={connected} activeTarget={config?.nft_contract || undefined} />

      {loadError && <div className={styles.banner}>config load failed: {loadError}</div>}

      <main className={styles.grid}>
        <div className={styles.left}>
          <WalletGrid wallets={wallets} />
          <CopyOpportunities
            opportunities={[...copyOpportunities.values()]}
            allowManualFire={config?.copymint_auto_fire_paid ?? false}
          />
          <EventFeed lines={lines} />
        </div>

        <div className={styles.right}>
          <TriggerConsole armed={armed} />
          <TargetResolver onTargetSet={(nftContract) => setConfig((c) => (c ? { ...c, nft_contract: nftContract } : c))} />
          {config && (
            <ConfigPanel config={config} onSaved={setConfig} disabled={armed} />
          )}
        </div>
      </main>
    </div>
  );
}
