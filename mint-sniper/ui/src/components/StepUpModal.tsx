import { useEffect, useRef, useState } from 'react';
import styles from './StepUpModal.module.css';
import { registerStepUpPrompter } from '../lib/api';

/**
 * Step-up auth (10f/10h). Registers itself as the app's ONE step-up
 * prompter at mount — see api.ts's registerStepUpPrompter doc comment
 * for why this is a module-level imperative bridge rather than a React
 * context: arm/fire/config-save/target-set/copymint-fire calls live in
 * several unrelated components (TriggerConsole, ConfigPanel,
 * TargetResolver, CopyOpportunities), and every one of them needs to be
 * able to trigger this same modal without each importing and rendering
 * its own copy. Mount this ONCE, at the app root, alongside — not
 * inside — the normal control-deck layout, since it needs to interrupt
 * ANY of those panels.
 */
export function StepUpModal() {
  const [pending, setPending] = useState<{ resolve: (code: string) => void; reject: (e: Error) => void } | null>(null);
  const [code, setCode] = useState('');
  const [error, setError] = useState<string | undefined>(undefined);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    registerStepUpPrompter(
      (errorMessage) =>
        new Promise<string>((resolve, reject) => {
          setCode('');
          setError(errorMessage);
          setPending({ resolve, reject });
        }),
    );
    return () => registerStepUpPrompter(null);
  }, []);

  useEffect(() => {
    if (pending) inputRef.current?.focus();
  }, [pending]);

  if (!pending) return null;

  function submit(e: React.FormEvent) {
    e.preventDefault();
    const trimmed = code.trim();
    if (!/^\d{6}$/.test(trimmed)) {
      setError('enter the 6-digit code from your authenticator app');
      return;
    }
    pending?.resolve(trimmed);
    setPending(null);
  }

  function cancel() {
    pending?.reject(new Error('step-up cancelled'));
    setPending(null);
  }

  return (
    <div className={styles.overlay} role="presentation" onClick={cancel}>
      <form
        className={styles.modal}
        role="dialog"
        aria-modal="true"
        aria-labelledby="step-up-title"
        onClick={(e) => e.stopPropagation()}
        onSubmit={submit}
      >
        <h2 id="step-up-title" className={styles.title}>
          STEP-UP REQUIRED
        </h2>
        <p className={styles.body}>
          This action moves money or changes what fires. Enter a fresh code from your
          authenticator app to continue — an already-open session isn't enough on its own.
        </p>
        <input
          ref={inputRef}
          className={styles.input}
          value={code}
          onChange={(e) => setCode(e.target.value.replace(/[^0-9]/g, '').slice(0, 6))}
          inputMode="numeric"
          autoComplete="one-time-code"
          placeholder="000000"
          maxLength={6}
        />
        {error && <p className={styles.error}>{error}</p>}
        <div className={styles.actions}>
          <button type="button" className={styles.cancelBtn} onClick={cancel}>
            CANCEL
          </button>
          <button type="submit" className={styles.confirmBtn} disabled={code.length !== 6}>
            CONFIRM
          </button>
        </div>
      </form>
    </div>
  );
}
