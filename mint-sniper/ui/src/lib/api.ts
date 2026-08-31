import type {
  AuthSession,
  Config,
  CopymintEligibilityReport,
  DelegatedPreflightResult,
  DelegatedStatus,
  ResolvedTarget,
  SearchHit,
  StatusResponse,
  TotpSetupMaterial,
  WebauthnDevice,
} from '../types';
import type {
  CreationChallengeResponseJSON,
  PublicKeyCredentialJSON,
  RegisterPublicKeyCredentialJSON,
  RequestChallengeResponseJSON,
} from './webauthnBrowser';

// Bootstrapped once at app startup from GET /api/token — the one route the
// Rust backend doesn't require the token on, since this is how the token
// reaches the UI in the first place. Never hardcoded, never fetched from
// anywhere but this same backend. See ui/README.md's security note for
// what this token does and doesn't protect against. Only actually
// enforced by the backend when identity (step 10) is NOT configured —
// see auth.rs's require_token_or_session — but harmless to keep sending
// either way.
let authToken: string | null = null;

export async function initAuth(): Promise<void> {
  const res = await fetch('/api/token');
  if (!res.ok) throw new Error(`failed to fetch API token: ${res.status}`);
  const data = (await res.json()) as { token: string };
  authToken = data.token;
}

export function getAuthToken(): string | null {
  return authToken;
}

function authHeaders(extra?: Record<string, string>): Record<string, string> {
  return {
    ...extra,
    ...(authToken ? { Authorization: `Bearer ${authToken}` } : {}),
  };
}

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}: ${await res.text().catch(() => '')}`);
  }
  return res.json() as Promise<T>;
}

// --- step-up auth (10f/10h) ---
//
// Thrown by the five fund-moving calls below when the backend's
// require_step_up (api.rs) rejects a request — see that function's doc
// comment for the exact status-code contract this mirrors: 428 means no
// code was sent at all, 412 means one was sent but was wrong/expired/
// already used. Both are distinct from a network/validation error, which
// is why this is its own Error subclass rather than a generic throw.
export class StepUpRequiredError extends Error {
  constructor(public reason: 'missing' | 'invalid') {
    super(reason === 'missing' ? 'a fresh authenticator code is required for this action' : 'that code was incorrect, expired, or already used');
  }
}

async function assertStepUpOk(res: Response): Promise<Response> {
  if (res.status === 428) throw new StepUpRequiredError('missing');
  if (res.status === 412) throw new StepUpRequiredError('invalid');
  return res;
}

// Registered by StepUpModal.tsx at app root — see that component for why
// this imperative module-level bridge exists instead of threading a
// context through every fire-adjacent button: any call in this file can
// trigger the prompt, not just ones rendered under a particular provider.
// Called with an error message when re-prompting after a wrong code, so
// the modal can show it without a caller having to plumb that through.
type StepUpPrompter = (errorMessage?: string) => Promise<string>;
let stepUpPrompter: StepUpPrompter | null = null;
export function registerStepUpPrompter(fn: StepUpPrompter | null): void {
  stepUpPrompter = fn;
}

// Wraps a fund-moving call: try without a code first (the common case —
// a session that already stepped up recently for something else still
// needs a FRESH code per action, so this will usually prompt), and on a
// wrong code, re-prompt rather than giving up after one attempt — a typo
// shouldn't mean starting the whole action over.
async function withStepUp<T>(doRequest: (stepUpCode?: string) => Promise<T>): Promise<T> {
  try {
    return await doRequest();
  } catch (e) {
    if (!(e instanceof StepUpRequiredError) || !stepUpPrompter) throw e;
  }
  let errorMessage: string | undefined;
  for (;;) {
    const code = await stepUpPrompter(errorMessage); // rejects (user cancelled) -> propagates out of withStepUp
    try {
      return await doRequest(code);
    } catch (e) {
      if (e instanceof StepUpRequiredError && e.reason === 'invalid') {
        errorMessage = e.message;
        continue;
      }
      throw e;
    }
  }
}

function stepUpHeader(code?: string): Record<string, string> {
  return code ? { 'X-Step-Up-Totp': code } : {};
}

export const api = {
  getConfig: () => fetch('/api/config', { headers: authHeaders() }).then((r) => json<Config>(r)),

  putConfig: (cfg: Config) =>
    withStepUp((code) =>
      fetch('/api/config', {
        method: 'PUT',
        headers: authHeaders({ 'Content-Type': 'application/json', ...stepUpHeader(code) }),
        body: JSON.stringify(cfg),
      })
        .then(assertStepUpOk)
        .then((r) => {
          if (!r.ok) throw new Error(`save failed: ${r.status}`);
        }),
    ),

  getStatus: () => fetch('/api/status', { headers: authHeaders() }).then((r) => json<StatusResponse>(r)),

  arm: () =>
    withStepUp((code) =>
      fetch('/api/arm', { method: 'POST', headers: authHeaders(stepUpHeader(code)) }).then(assertStepUpOk),
    ),
  abort: () => fetch('/api/abort', { method: 'POST', headers: authHeaders() }),
  fireNow: () =>
    withStepUp((code) =>
      fetch('/api/trigger', { method: 'POST', headers: authHeaders(stepUpHeader(code)) }).then(assertStepUpOk),
    ),

  // Server independently re-verifies liveness + max_copymint_price_wei
  // fresh via getPublicDrop before firing — never trusts the price this
  // client is currently displaying. See api.rs's post_copymint_fire.
  // Resolves with the server's plain-text confirmation/rejection message.
  fireCopymint: (nftContract: string, feeRecipient: string): Promise<string> =>
    withStepUp(async (code) => {
      const res = await fetch('/api/copymint/fire', {
        method: 'POST',
        headers: authHeaders({ 'Content-Type': 'application/json', ...stepUpHeader(code) }),
        body: JSON.stringify({ nft_contract: nftContract, fee_recipient: feeRecipient }),
      }).then(assertStepUpOk);
      const text = await res.text();
      if (!res.ok) throw new Error(text || `${res.status} ${res.statusText}`);
      return text;
    }),

  // Read-only — resolves + verifies without changing any bot state. See
  // api.rs's post_target_resolve. Not step-up-gated: nothing is committed.
  resolveTarget: (input: string) =>
    fetch('/api/target/resolve', {
      method: 'POST',
      headers: authHeaders({ 'Content-Type': 'application/json' }),
      body: JSON.stringify({ input }),
    }).then((r) => json<ResolvedTarget>(r)),

  // Server re-verifies fresh (never trusts this client's earlier
  // /resolve result) before committing. See api.rs's post_target_set.
  setTarget: (nftContract: string) =>
    withStepUp((code) =>
      fetch('/api/target/set', {
        method: 'POST',
        headers: authHeaders({ 'Content-Type': 'application/json', ...stepUpHeader(code) }),
        body: JSON.stringify({ nft_contract: nftContract }),
      })
        .then(assertStepUpOk)
        .then((r) => json<ResolvedTarget>(r)),
    ),

  // step 8c — returns unverified name-search candidates only; every one
  // still needs resolveTarget before it's usable. See api.rs's
  // post_target_search and opensea.rs's namesquatting-risk reasoning.
  searchTargets: (query: string) =>
    fetch('/api/target/search', {
      method: 'POST',
      headers: authHeaders({ 'Content-Type': 'application/json' }),
      body: JSON.stringify({ query }),
    }).then((r) => json<SearchHit[]>(r)),

  // step 31b — read-only pre-fire "check eligibility" action, never
  // step-up gated (nothing is committed, same reasoning as resolveTarget).
  // Never trusts a client-cached price/state either — the backend re-reads
  // getPublicDrop + getMintStats live for every call. See api.rs's
  // post_copymint_eligibility / copymint::check_eligibility.
  checkCopymintEligibility: (nftContract: string) =>
    fetch('/api/copymint/eligibility', {
      method: 'POST',
      headers: authHeaders({ 'Content-Type': 'application/json' }),
      body: JSON.stringify({ nft_contract: nftContract }),
    }).then((r) => json<CopymintEligibilityReport>(r)),

  // --- delegated mint mode (v1, DELEGATED_SERIAL) ---
  // status/preflight are both read-only — see api.rs's own doc comments
  // on get_delegated_status/post_delegated_preflight. Neither ever
  // returns anything beyond public addresses; see
  // api::delegated_secrets_tests for the tests that assert this.
  getDelegatedStatus: () => fetch('/api/delegated/status', { headers: authHeaders() }).then((r) => json<DelegatedStatus>(r)),

  preflightDelegated: () =>
    fetch('/api/delegated/preflight', { method: 'POST', headers: authHeaders() }).then((r) => json<DelegatedPreflightResult>(r)),

  // Same sensitivity class as arm()/fireNow() — step-up gated, no
  // request body (everything is re-read from config and re-derived from
  // the mnemonic fresh, server-side, at fire time). See api.rs's
  // post_delegated_fire doc comment.
  fireDelegated: () =>
    withStepUp((code) =>
      fetch('/api/delegated/fire', { method: 'POST', headers: authHeaders(stepUpHeader(code)) }).then(assertStepUpOk),
    ),

  // --- identity (step 10c-10h) ---
  // None of these go through authHeaders()/the bearer token — they're
  // how a session gets established/progressed in the first place (see
  // api.rs's router doc comment), and rely on the browser's normal
  // same-origin cookie handling instead.

  getAuthSession: () => fetch('/auth/session').then((r) => json<AuthSession>(r)),
  logout: () => fetch('/auth/logout', { method: 'POST' }),

  totpSetupStart: () => fetch('/auth/totp/setup/start', { method: 'POST' }).then((r) => json<TotpSetupMaterial>(r)),
  totpSetupVerify: (code: string) =>
    fetch('/auth/totp/setup/verify', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ code }),
    }),
  // Per-login check for an ALREADY-enrolled account — see totp.rs's
  // verify_login. Distinct from setup: this never touches or regenerates
  // the stored secret.
  totpVerify: (code: string) =>
    fetch('/auth/totp/verify', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ code }),
    }),

  webauthnRegisterStart: () => fetch('/auth/webauthn/register/start', { method: 'POST' }).then((r) => json<CreationChallengeResponseJSON>(r)),
  webauthnRegisterFinish: (deviceLabel: string, credential: RegisterPublicKeyCredentialJSON) =>
    fetch('/auth/webauthn/register/finish', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ device_label: deviceLabel, credential }),
    }),
  webauthnAuthenticateStart: () =>
    fetch('/auth/webauthn/authenticate/start', { method: 'POST' }).then((r) => json<RequestChallengeResponseJSON>(r)),
  webauthnAuthenticateFinish: (credential: PublicKeyCredentialJSON) =>
    fetch('/auth/webauthn/authenticate/finish', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(credential),
    }),
  getWebauthnDevices: () => fetch('/auth/webauthn/devices').then((r) => json<WebauthnDevice[]>(r)),
  revokeWebauthnDevice: (id: string) => fetch(`/auth/webauthn/devices/${encodeURIComponent(id)}/revoke`, { method: 'POST' }),
};
