import { describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { OperatorPanel, type DelegatedRunState } from './OperatorPanel';
import type { Config, DelegatedStatus } from '../types';

// Delegated mint mode (v1) — a real, running assertion that mnemonic and
// private-key material can never reach OperatorPanel.tsx's props, state,
// or rendered DOM, mirroring api.rs's delegated_secrets_tests module on
// the backend (same standard, applied at the UI boundary). This is NOT
// a secret this test is trying to keep — it's a standard, public BIP-39
// test phrase, injected here specifically to prove that EVEN IF it
// somehow reached this component (it structurally cannot — see
// OperatorPanel.tsx's own doc comment), nothing in the component's own
// code path would ever render it.
const FORBIDDEN_MNEMONIC = 'test test test test test test test test test test test junk';
const FORBIDDEN_MNEMONIC_WORDS = FORBIDDEN_MNEMONIC.split(' ');
// A raw secp256k1 private key is exactly 64 hex chars — distinct from a
// 40-hex-char address, which is expected and fine to render.
const PRIVATE_KEY_HEX_RUN_LENGTH = 64;

vi.mock('../lib/api', () => ({
  api: {
    getDelegatedStatus: vi.fn(),
    preflightDelegated: vi.fn(),
    fireDelegated: vi.fn(),
  },
}));

import { api } from '../lib/api';

function baseConfig(overrides: Partial<Config> = {}): Config {
  return {
    ws_rpc_url: '',
    http_rpc_urls: [],
    mint_mode: 'seadrop',
    contract_address: '',
    mint_fn_signature: '',
    mint_fn_args_template: [],
    mint_state_fn_signature: '',
    trigger_mode: 'timestamp',
    trigger_timestamp_unix: 0,
    mint_enable_admin: '',
    seadrop_address: '',
    nft_contract: '0x00000000000000000000000000000000001234',
    fee_recipient: '0x0000a26b00c1F0DF003000390027140000fAa719',
    quantity_per_wallet: 1,
    priority_fee_multiplier: 1,
    max_priority_fee_gwei_cap: 1,
    gas_limit_headroom_pct: 0,
    jitter_ms_min: 0,
    jitter_ms_max: 0,
    gas_jitter_pct: 0,
    wallets: [],
    tracked_wallets: [],
    copymint_auto_fire_free: true,
    copymint_auto_fire_paid: false,
    max_copymint_price_wei: 0,
    opensea_api_key_env: '',
    mint_execution: 'delegated',
    delegate_mnemonic_env: 'OPERATOR_MNEMONIC',
    delegate_count: 3,
    ...overrides,
  };
}

// Two real receiver addresses, derived independently offline from
// FORBIDDEN_MNEMONIC at m/44'/60'/0'/0/1 and /2 — public data, fine to
// hardcode. What must NEVER appear anywhere near this component is the
// mnemonic phrase itself or a 64-hex-char private key.
const FIXTURE_STATUS: DelegatedStatus = {
  operator_address: '0x9858EfFD232B4033E47d90003D41EC34EcaEda94',
  operator_balance_eth: '0.42',
  delegate_count: 2,
  receivers_derived: true,
  max_delegates: 200,
  receivers: [
    { index: 1, address: '0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266' },
    { index: 2, address: '0x70997970C51812dc3A010C7d01b50e0d17dc79C8' },
  ],
  mode_label: 'DELEGATED_SERIAL',
};

function scanForForbiddenSecrets(container: HTMLElement) {
  const text = container.textContent ?? '';
  const html = container.innerHTML;

  for (const word of FORBIDDEN_MNEMONIC_WORDS) {
    expect(text).not.toContain(word);
    expect(html).not.toContain(word);
  }
  expect(text).not.toContain(FORBIDDEN_MNEMONIC);

  // Any contiguous hex run of exactly private-key length, anywhere in
  // the rendered output — not just an exact string match, in case a key
  // were ever embedded inside a larger string (e.g. concatenated into a
  // URL or debug blob).
  const hexRuns = html.match(/[0-9a-fA-F]{40,}/g) ?? [];
  for (const run of hexRuns) {
    // A 40-hex-char address is expected and fine; anything landing on
    // exactly 64 (or padding past it, e.g. `0x` + 64 hex) is suspicious.
    expect(run.length, `found a ${run.length}-char hex run: ${run}`).not.toBe(PRIVATE_KEY_HEX_RUN_LENGTH);
    expect(run.length, `found a ${run.length}-char hex run: ${run}`).not.toBe(PRIVATE_KEY_HEX_RUN_LENGTH + 2);
  }
}

describe('OperatorPanel — never renders mnemonic or private key material', () => {
  it('renders operator/receiver status with no secret material, pre-fire', async () => {
    vi.mocked(api.getDelegatedStatus).mockResolvedValue(FIXTURE_STATUS);
    vi.mocked(api.preflightDelegated).mockResolvedValue({
      outcome: 'ok',
      estimated_max_spend_wei: '1000000000000000',
      delegate_count: 2,
    });

    const { container } = render(<OperatorPanel config={baseConfig()} run={null} onRunReset={() => {}} />);

    await waitFor(() => expect(screen.getByText(/OPERATOR PANEL/)).toBeInTheDocument());
    await waitFor(() => expect(screen.getByText(FIXTURE_STATUS.operator_address)).toBeInTheDocument());

    scanForForbiddenSecrets(container);
  });

  it('renders no secret material in a live/complete run view either', async () => {
    vi.mocked(api.getDelegatedStatus).mockResolvedValue(FIXTURE_STATUS);

    const run: DelegatedRunState = {
      delegateCount: 2,
      estimatedMaxSpendWei: '1000000000000000',
      results: new Map([
        [1, { address: FIXTURE_STATUS.receivers[0].address, success: true, detail: '0xabc123deadbeef' }],
        [
          2,
          {
            address: FIXTURE_STATUS.receivers[1].address,
            success: false,
            detail: 'execution reverted: MinterNotAllowed',
          },
        ],
      ]),
      complete: { minted: 1, attempted: 2, totalCostWei: '900000000000000' },
    };

    const { container } = render(<OperatorPanel config={baseConfig()} run={run} onRunReset={() => {}} />);

    await waitFor(() => expect(screen.getByText(/DELEGATED_SERIAL RUN/)).toBeInTheDocument());
    await waitFor(() => expect(screen.getByText(/Delegated mint complete/)).toBeInTheDocument());

    scanForForbiddenSecrets(container);
  });

  it('renders no secret material when the preflight is refused (minter mismatch)', async () => {
    vi.mocked(api.getDelegatedStatus).mockResolvedValue(FIXTURE_STATUS);
    vi.mocked(api.preflightDelegated).mockResolvedValue({
      outcome: 'minter_mismatch',
      revert_reason: 'execution reverted: MinterNotAllowed',
    });

    const { container, getByText } = render(<OperatorPanel config={baseConfig()} run={null} onRunReset={() => {}} />);

    await waitFor(() => expect(screen.getByText(FIXTURE_STATUS.operator_address)).toBeInTheDocument());
    getByText('RUN PREFLIGHT').click();

    await waitFor(() => expect(screen.getByText(/REFUSED TO ARM/)).toBeInTheDocument());

    scanForForbiddenSecrets(container);
  });
});
