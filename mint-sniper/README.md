# mint-sniper

Multi-wallet free-mint sniper for EVM contracts (Ethereum mainnet + L2s).
Targets a project's own mint contract directly — **not** OpenSea's Drop
infrastructure (see caveat below).

## Setup

```bash
cargo build --release
cp config.example.toml config.toml
# fill in RPC URLs and contract details in config.toml
export SNIPER_PK_1=0x...
export SNIPER_PK_2=0x...
export SNIPER_PK_3=0x...
```

## Before you point this at a real mint

1. ~~Compile and fix API drift.~~ Done — see `CLAUDE.md`'s known-gaps
   list. `cargo build`/`cargo check` are clean with zero warnings.
2. ~~Dry-run on a testnet clone of the target contract.~~ Done against
   Sepolia — see `CLAUDE.md`'s "Testnet dry run (step 5)" section for
   what was verified (timestamp mode firing real mints, the
   deliberate-revert path reporting correctly, the UI against a live
   bot) and what couldn't be exercised in that specific sandboxed run
   (poll_state/mempool_watch's WS-based watchers, blocked by that
   environment's TLS-interception proxy, not by anything in this repo —
   their subscription protocols were independently confirmed working
   against the same RPC outside the bot). SeaDrop 1.0 is confirmed live
   on Sepolia at the same singleton address as mainnet (verified via
   `eth_getCode`, not just the README table) — no need to deploy the
   singleton yourself, only a test `ERC721SeaDrop` token pointed at it.
3. **Confirm the mint isn't allowlist-gated.** If `mint()` requires a merkle
   proof argument (custom mode) or the SeaDrop drop is allowlist/signed/
   token-gated rather than public (seadrop mode — check `getPublicDrop`
   returns a nonzero `startTime`, or that the project has announced a
   public stage at all), blind sniping fails at the revert, not at your
   code.
4. **Fund wallets from varied sources at varied times.** Same funding tx,
   same block, same gas price across N wallets is a one-line sybil-detection
   heuristic — costs you allowlist eligibility on the *next* drop from that
   project even if this mint succeeds.
5. **Verify `mint_state_fn_signature` return decoding** (custom mode only —
   seadrop mode doesn't use this, it reads real timing from `getPublicDrop`
   instead). The skeleton assumes a plain `bool` return. Some contracts
   expose sale state as an enum or a struct with a start timestamp — adjust
   the decode logic in `watcher.rs::run_state_poll_watcher`.

## Control deck (PWA)

The bot now exposes an HTTP + WebSocket API (`src/api.rs`, bound to
`127.0.0.1:4117`) so the `ui/` PWA can edit config, watch wallet
balances/nonces live, stream the event log, and arm/disarm/fire without
touching `config.toml` or a terminal. See `ui/README.md` for the dev/prod
workflow. Architecture: `bus.rs` (typed event broadcast) →
`state.rs` (shared state) → `api.rs` (routes) → `main.rs::control_loop`
(single-writer arm/disarm/fire state machine, owns the wallet signers).

Run both together for dev:

```bash
cargo run                 # terminal 1 — bot + API on :4117
cd ui && npm install && npm run dev   # terminal 2 — PWA on :5173
```

Security note: this has changed since step 7b — the API is no longer
unauthenticated. Every route requires either a local bearer token (step
7b, the default when identity isn't configured) or a real per-identity
session (Google Sign-In + TOTP + WebAuthn, step 10) — see
`ui/README.md`'s "Security model" section for the exact boundary either
mode does and doesn't protect. It still binds to `127.0.0.1` only,
unconditionally, regardless of auth mode or where it's deployed — the
token/session model narrows who can act once a request arrives, it was
never a substitute for not exposing the port directly. As of step 15
this bot runs on a real VPS for the first time (see `DEPLOY.md`) to get
better RPC latency; reaching it from elsewhere goes through Tailscale
or a Cloudflare Tunnel + Access (step 10.5, see `ui/README.md`'s
"Reaching this from your phone" section), never by exposing 4117
directly to the internet.

## Mint modes

- **`mint_mode = "custom"`** (default) — arbitrary project mint() contract,
  as originally built. You supply the ABI signature and target address.
- **`mint_mode = "seadrop"`** — OpenSea's own open-source, Spearbit-audited
  SeaDrop protocol. Public-stage mints go through one fixed singleton
  contract (`0x00005EA00Ac477B1030CE78506496e8C2dE24bf5` on Ethereum
  mainnet and Polygon) with a fixed ABI (`mintPublic`) — no per-project
  guessing required. The bot reads `getPublicDrop(nftContract)` at boot to
  get the real start time and price, and auto-arms for that exact second.
  See `src/seadrop.rs` for the full mechanism and its scope limit: this
  only covers the *public* stage — allowlist, token-gated, and
  server-signed SeaDrop mints need data (a merkle proof, a held token, a
  project-issued signature) this bot doesn't have and can't generate.

## What this does NOT do

- Does not script SeaDrop's allowlist (`mintAllowList`), token-gated
  (`mintAllowedTokenHolder`), or server-signed (`mintSigned`) stages — see
  above. Only the no-external-data public stage is covered.
- Does not touch a non-SeaDrop project's bespoke frontend/API flow if one
  exists outside the on-chain contract (rare now that SeaDrop covers most
  OpenSea-launched drops, but some projects still run custom infra).
- `trigger_mode = "mempool_watch"` fires on a pending (not yet confirmed)
  tx from a configured admin address to the watched contract — see
  `mint_enable_admin` in `config.example.toml` and `watcher.rs`'s
  `run_mempool_watcher` doc comment for exactly which contract that is in
  seadrop mode, and for why it matches on (from, to) rather than decoding
  a specific function call. Needs a WebSocket RPC with full mempool
  visibility (Geth 1.11+, `newPendingTransactions` with full tx bodies) —
  many free/public RPC providers don't expose this even over WS, and
  arming fails loudly (auto-disarm + a clear error) rather than silently
  watching nothing if the subscription isn't supported. **Finding
  `mint_enable_admin` for a project you don't control:** don't use the
  deployer address from Etherscan's "Contract Creator" field — verified
  against SeaDrop's actual source that ownership is a `TwoStepOwnable`
  (transferable), and moving ownership from a deploy wallet to a cold
  wallet/multisig before the drop goes live is routine, not an edge case.
  Call the nft contract's `owner()` view function yourself, close to
  arm-time, since it can change again after you check.
- Does not manage wallet funding/withdrawal logistics — that's a separate,
  equally important operational concern (gas top-ups, consolidating minted
  NFTs to a single wallet post-mint, etc.) left out of scope here.
