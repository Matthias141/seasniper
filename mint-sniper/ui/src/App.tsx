import { useCallback, useEffect, useState } from 'react';
import styles from './App.module.css';
import { StatusBar } from './components/StatusBar';
import { WalletGrid } from './components/WalletGrid';
import { EventFeed, type FeedLine } from './components/EventFeed';
import { ConfigPanel } from './components/ConfigPanel';
import { TriggerConsole } from './components/TriggerConsole';
import { CopyOpportunities, type CopyOpportunity } from './components/CopyOpportunities';
import { OperatorPanel, type DelegatedRunState } from './components/OperatorPanel';
import { TargetResolver } from './components/TargetResolver';
import { LoginScreen } from './components/LoginScreen';
import { IdentitySetup } from './components/IdentitySetup';
import { StepUpModal } from './components/StepUpModal';
import { useEventSocket } from './lib/useEventSocket';
import { api, initAuth } from './lib/api';
import type { AuthSession, Config, ServerEvent, WalletStatus } from './types';

let lineId = 0;

// Gates the whole app behind GET /api/token resolving first — the WS hook
// and every api.ts call need the token already in memory before they fire,
// not racing it. A failed fetch here means the backend isn't reachable at
// all, which is worth a distinct message from "config load failed". Once
// that's ready, AuthGate (step 10h) decides what to actually show:
// unchanged Control if identity isn't configured for this instance
// (bearer-token-only, exactly pre-step-10 behavior), otherwise a real
// login/setup wall in front of it. StepUpModal mounts once here, at the
// true app root — see its own doc comment for why it can't live inside
// Control alongside the panels that trigger it.
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

  return (
    <>
      <AuthGate />
      <StepUpModal />
    </>
  );
}

function AuthGate() {
  const [session, setSession] = useState<AuthSession | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  const refetch = useCallback(() => {
    api
      .getAuthSession()
      .then(setSession)
      .catch((e) => setLoadError(e instanceof Error ? e.message : 'failed to check sign-in state'));
  }, []);

  useEffect(refetch, [refetch]);

  if (loadError) {
    return (
      <div className={styles.shell}>
        <div className={styles.banner}>couldn't check sign-in state: {loadError}</div>
      </div>
    );
  }
  if (!session) return null;

  // Identity not configured for this instance at all — exactly pre-step-10
  // behavior, the bearer token alone gates the API (see auth.rs's
  // require_token_or_session). Don't show a login wall for a mode that
  // has no login to perform.
  if (!session.identity_configured) return <Control />;

  if (!session.signed_in) return <LoginScreen />;
  if (!session.admin_tier) return <IdentitySetup session={session} onProgressed={refetch} />;

  return <Control onSignOut={() => api.logout().then(refetch)} />;
}

function Control({ onSignOut }: { onSignOut?: () => void }) {
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
  // Delegated mint mode (v1, DELEGATED_SERIAL) — lifted up here rather
  // than owned inside OperatorPanel.tsx so it drives off the same single
  // WS connection every other panel already shares (see
  // useEventSocket.ts), same pattern as copyOpportunities above.
  const [delegatedRun, setDelegatedRun] = useState<DelegatedRunState | null>(null);

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
        case 'mint_result': {
          // step 13e — real per-attempt timing, shown alongside
          // success/failure rather than only in the audit log, so the
          // control deck's own feed is where you'd actually check "how
          // fast did that fire" right after it happens.
          const timing = [
            `prepare age ${event.prepare_age_ms}ms`,
            event.trigger_to_dispatch_ms != null ? `trigger→dispatch ${event.trigger_to_dispatch_ms}ms` : null,
            event.send_to_ack_ms != null ? `send→ack ${event.send_to_ack_ms}ms` : null,
            event.dispatch_to_inclusion_ms != null ? `dispatch→inclusion ${event.dispatch_to_inclusion_ms}ms` : null,
          ]
            .filter(Boolean)
            .join(', ');
          pushLine(
            event.success ? 'info' : 'error',
            `${event.address}: ${event.success ? 'confirmed' : 'failed'} — ${event.detail}${timing ? ` (${timing})` : ''}`,
          );
          break;
        }
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
        // --- delegated mint mode (v1, DELEGATED_SERIAL) --- see
        // bus.rs's ServerEvent doc comment. Never carries anything
        // beyond a receiver's public address.
        case 'delegated_run_started':
          setDelegatedRun({
            delegateCount: event.delegate_count,
            estimatedMaxSpendWei: event.estimated_max_spend_wei,
            results: new Map(),
            complete: null,
          });
          pushLine('warn', `DELEGATED_SERIAL run started — ${event.delegate_count} receivers`);
          break;
        case 'delegated_mint_result':
          setDelegatedRun((prev) =>
            prev
              ? {
                  ...prev,
                  results: new Map(prev.results).set(event.receiver_index, {
                    address: event.receiver_address,
                    success: event.success,
                    detail: event.detail,
                  }),
                }
              : prev,
          );
          pushLine(
            event.success ? 'info' : 'error',
            `DELEGATED_SERIAL #${event.receiver_index} ${event.receiver_address}: ${event.success ? 'confirmed' : 'failed'} — ${event.detail}`,
          );
          break;
        case 'delegated_run_complete':
          setDelegatedRun((prev) =>
            prev
              ? { ...prev, complete: { minted: event.minted, attempted: event.attempted, totalCostWei: event.total_cost_wei } }
              : prev,
          );
          pushLine('warn', `DELEGATED_SERIAL run complete — ${event.minted}/${event.attempted} minted`);
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
      <StatusBar armed={armed} connected={connected} activeTarget={config?.nft_contract || undefined} onSignOut={onSignOut} />

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
          {config?.mint_execution === 'delegated' && (
            <OperatorPanel config={config} run={delegatedRun} onRunReset={() => setDelegatedRun(null)} />
          )}
          <TargetResolver onTargetSet={(nftContract) => setConfig((c) => (c ? { ...c, nft_contract: nftContract } : c))} />
          {config && (
            <ConfigPanel config={config} onSaved={setConfig} disabled={armed} />
          )}
        </div>
      </main>
    </div>
  );
}
