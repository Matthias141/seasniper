import { useState } from 'react';
import styles from './IdentitySetup.module.css';
import { api } from '../lib/api';
import { runAuthenticationCeremony, runRegistrationCeremony, webauthnSupported } from '../lib/webauthnBrowser';
import type { AuthSession } from '../types';

/**
 * Progresses a signed-in-but-not-yet-admin_tier session through TOTP
 * then WebAuthn (step 10c-10h) — one stage at a time, driven entirely by
 * `session`'s own flags rather than local stage state, so a page reload
 * mid-flow resumes at the right step instead of restarting.
 *
 * `totp_enrolled`/`webauthn_enrolled` (account-wide: has a factor ever
 * been set up) vs `totp_verified`/`webauthn_verified` (THIS session's
 * own login progress) are genuinely different things — see api.rs's
 * get_auth_session doc comment. Getting this branch wrong would mean
 * either re-registering a TOTP secret on every ordinary login (silently
 * breaking the operator's existing authenticator entry) or asking a
 * brand-new account to "just enter a code" with nothing to enter it
 * from — both are real, considered failure modes, not just churn.
 */
export function IdentitySetup({ session, onProgressed }: { session: AuthSession; onProgressed: () => void }) {
  if (!session.totp_verified) {
    return session.totp_enrolled ? <TotpVerifyStep onDone={onProgressed} /> : <TotpSetupStep onDone={onProgressed} />;
  }
  if (!session.webauthn_verified) {
    return session.webauthn_enrolled ? <WebauthnAuthenticateStep onDone={onProgressed} /> : <WebauthnRegisterStep onDone={onProgressed} />;
  }
  // Both stages report verified but App.tsx hasn't seen admin_tier flip
  // yet (a refetch is in flight) — render nothing rather than a stale step.
  return null;
}

function StepShell({
  step,
  total,
  title,
  children,
}: {
  step: number;
  total: number;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className={styles.shell}>
      <div className={styles.panel}>
        <div className={styles.progress}>
          STEP {step} OF {total}
        </div>
        <h1 className={styles.heading}>{title}</h1>
        {children}
      </div>
    </div>
  );
}

function TotpSetupStep({ onDone }: { onDone: () => void }) {
  const [material, setMaterial] = useState<{ qr_data_uri: string; secret_base32: string } | null>(null);
  const [code, setCode] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function start() {
    setError(null);
    setBusy(true);
    try {
      setMaterial(await api.totpSetupStart());
    } catch (e) {
      setError(e instanceof Error ? e.message : 'failed to start TOTP setup');
    } finally {
      setBusy(false);
    }
  }

  async function verify(e: React.FormEvent) {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      const res = await api.totpSetupVerify(code.trim());
      if (!res.ok) throw new Error((await res.text()) || 'incorrect code');
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'verification failed');
    } finally {
      setBusy(false);
    }
  }

  return (
    <StepShell step={1} total={2} title="SET UP TWO-FACTOR AUTH">
      {!material ? (
        <>
          <p className={styles.body}>
            No authenticator is enrolled for this account yet. Generate a secret and scan it
            into Google Authenticator, 1Password, or any TOTP app.
          </p>
          <button className={styles.primaryBtn} onClick={start} disabled={busy}>
            {busy ? 'GENERATING…' : 'GENERATE SECRET'}
          </button>
        </>
      ) : (
        <form onSubmit={verify} className={styles.form}>
          <img className={styles.qr} src={material.qr_data_uri} alt="TOTP setup QR code" width={220} height={220} />
          <p className={styles.body}>
            Can't scan? Enter this manually: <code className={styles.secret}>{material.secret_base32}</code>
          </p>
          <p className={styles.body}>
            This won't be shown as "enabled" until you enter one real code below — proving your
            app actually has it working.
          </p>
          <input
            className={styles.codeInput}
            value={code}
            onChange={(e) => setCode(e.target.value.replace(/[^0-9]/g, '').slice(0, 6))}
            inputMode="numeric"
            autoComplete="one-time-code"
            placeholder="000000"
            maxLength={6}
            autoFocus
          />
          {error && <p className={styles.error}>{error}</p>}
          <button className={styles.primaryBtn} type="submit" disabled={busy || code.length !== 6}>
            {busy ? 'VERIFYING…' : 'VERIFY & ENABLE'}
          </button>
        </form>
      )}
      {error && !material && <p className={styles.error}>{error}</p>}
    </StepShell>
  );
}

function TotpVerifyStep({ onDone }: { onDone: () => void }) {
  const [code, setCode] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      const res = await api.totpVerify(code.trim());
      if (!res.ok) throw new Error((await res.text()) || 'incorrect or already-used code');
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'verification failed');
      setCode('');
    } finally {
      setBusy(false);
    }
  }

  return (
    <StepShell step={1} total={2} title="ENTER YOUR CODE">
      <form onSubmit={submit} className={styles.form}>
        <p className={styles.body}>Enter the current code from your authenticator app.</p>
        <input
          className={styles.codeInput}
          value={code}
          onChange={(e) => setCode(e.target.value.replace(/[^0-9]/g, '').slice(0, 6))}
          inputMode="numeric"
          autoComplete="one-time-code"
          placeholder="000000"
          maxLength={6}
          autoFocus
        />
        {error && <p className={styles.error}>{error}</p>}
        <button className={styles.primaryBtn} type="submit" disabled={busy || code.length !== 6}>
          {busy ? 'VERIFYING…' : 'CONTINUE'}
        </button>
      </form>
    </StepShell>
  );
}

function WebauthnRegisterStep({ onDone }: { onDone: () => void }) {
  const [label, setLabel] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function register() {
    if (!label.trim()) {
      setError('name this device first (e.g. "laptop")');
      return;
    }
    setError(null);
    setBusy(true);
    try {
      const ccr = await api.webauthnRegisterStart();
      const credential = await runRegistrationCeremony(ccr);
      const res = await api.webauthnRegisterFinish(label.trim(), credential);
      if (!res.ok) throw new Error((await res.text()) || 'registration rejected');
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'registration failed');
    } finally {
      setBusy(false);
    }
  }

  return (
    <StepShell step={2} total={2} title="REGISTER A DEVICE">
      {!webauthnSupported() ? (
        <p className={styles.error}>
          This browser doesn't support WebAuthn/passkeys — try a recent Chrome, Safari, Edge, or
          Firefox.
        </p>
      ) : (
        <>
          <p className={styles.body}>
            Register up to 2 admin devices (Face ID, Touch ID, Windows Hello, or a security key).
            You'll need this device (or the other one) to sign in from now on.
          </p>
          <input
            className={styles.textInput}
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            placeholder="device name (e.g. laptop)"
            maxLength={64}
            autoFocus
          />
          {error && <p className={styles.error}>{error}</p>}
          <button className={styles.primaryBtn} onClick={register} disabled={busy}>
            {busy ? 'WAITING FOR DEVICE…' : 'REGISTER THIS DEVICE'}
          </button>
        </>
      )}
    </StepShell>
  );
}

function WebauthnAuthenticateStep({ onDone }: { onDone: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function authenticate() {
    setError(null);
    setBusy(true);
    try {
      const rcr = await api.webauthnAuthenticateStart();
      const credential = await runAuthenticationCeremony(rcr);
      const res = await api.webauthnAuthenticateFinish(credential);
      if (!res.ok) throw new Error((await res.text()) || 'authentication rejected');
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'authentication failed');
    } finally {
      setBusy(false);
    }
  }

  return (
    <StepShell step={2} total={2} title="CONFIRM YOUR DEVICE">
      {!webauthnSupported() ? (
        <p className={styles.error}>
          This browser doesn't support WebAuthn/passkeys — sign in from a device where you
          registered one, or a recent Chrome, Safari, Edge, or Firefox.
        </p>
      ) : (
        <>
          <p className={styles.body}>Use your registered device to finish signing in.</p>
          {error && <p className={styles.error}>{error}</p>}
          <button className={styles.primaryBtn} onClick={authenticate} disabled={busy}>
            {busy ? 'WAITING FOR DEVICE…' : 'USE THIS DEVICE'}
          </button>
        </>
      )}
    </StepShell>
  );
}
