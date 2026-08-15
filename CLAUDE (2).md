# CLAUDE.md

Context for Claude Code working in this repo. Read this before making changes.

## What this is

Multi-wallet EVM free-mint sniper: Rust backend (bot + control API) + React
PWA control deck. Targets a project's own mint contract directly via
direct RPC calls — not OpenSea's Drop/Seaport infrastructure, which is a
different, out-of-scope problem (see "Explicitly out of scope" below).

Real money and gas move when this runs. Correctness and security review
standards apply to every change here — this is not a toy project.

## Status: UNTESTED SKELETON

Written without a Rust toolchain or npm in the authoring environment.
Nothing in `src/` or `ui/` has been compiled. Treat every alloy and axum
API call as "probably right, unverified" — both crates' surfaces shift
across minor versions. **The first task in any session on this repo should
be `cargo build` and `npm install && npm run typecheck`, then fixing
whatever drift surfaces**, before adding any new feature.

## Architecture

```
src/
  main.rs      — entrypoint; wires config, wallets, API server, control_loop
  config.rs    — Config struct, loads config.toml, resolves wallet private
                 keys from env vars (never stored in config.toml itself)
  wallet.rs    — ManagedWallet: signer + locally-tracked nonce per wallet
  watcher.rs   — detects the mint-live trigger (poll_state | timestamp;
                 mempool_watch is stubbed, not implemented)
  executor.rs  — fire_all_wallets: concurrent, jittered, multi-RPC broadcast
                 of the mint tx per wallet, with a per-wallet ETH value
                 (0 for free mints, SeaDrop public-stage price otherwise)
  seadrop.rs   — SeaDrop 1.0 ABI: reads getPublicDrop (price/timing) and
                 builds mintPublic calldata. Public stage only — see its
                 doc comment for why allowlist/signed/token-gated stages
                 aren't (and can't trivially be) covered.
  bus.rs       — ServerEvent enum + broadcast channel, single source of
                 truth for everything the UI displays in real time
  state.rs     — AppState (shared config/wallet-status/armed flag/bus/
                 control channel), ControlMsg enum (Arm/Disarm/FireNow)
  api.rs       — axum router: GET/PUT /api/config, GET /api/status,
                 POST /api/arm|/api/abort|/api/trigger, WS /ws/events

ui/            — Vite + React + TS PWA, "Terminal Command Deck" design
                 (see ui/src/styles/tokens.css for the token system)
```

Control flow: all arm/disarm/fire commands, whether from the API or from
the watcher's own auto-trigger, funnel through the single `control_loop`
task in `main.rs` via `ControlMsg` over an mpsc channel. This is
intentional — it's the one thing preventing two fires racing each other
if the UI double-clicks and the watcher triggers in the same instant.
**Do not add a second path that calls `executor::fire_all_wallets`
directly; route everything through `control_tx`.**

Wallet signers (i.e. private key material in memory) live only in
`control_loop`'s local scope in `main.rs` and inside `executor.rs`. They
never cross into `api.rs`, `state.rs`, or the UI. **Do not add a raw
private key field to any API request/response type** — env var *names*
only cross the API boundary, by design (see api.rs's doc comment).

## Commands

```bash
# Backend
cargo build              # first thing to run in a new session
cargo check               # faster iteration once building works
cargo run                 # starts bot + API on 127.0.0.1:4117

# Frontend
cd ui
npm install
npm run dev                # :5173, proxies /api and /ws to :4117
npm run typecheck
npm run build               # outputs ui/dist/
```

No test suite exists yet. If you add one: unit-test calldata encoding
(`encode_mint_calldata`/`encode_selector_only` in main.rs) and the gas
jitter math (`apply_pct_jitter` in executor.rs) first — those are pure
functions with no network dependency and the highest cost-of-being-wrong.

## Known gaps (fix before relying on this for a real mint)

1. **Nothing compiled yet.** Alloy 0.9.x / axum 0.7.x API drift is
   expected. Fix in place, don't rewrite around it unless a function
   genuinely no longer exists.
2. **`mempool_watch` trigger mode is not implemented** — `watcher.rs` only
   has `run_state_poll_watcher` and `run_timestamp_watcher`. Selecting it
   in config currently falls through to poll_state in `control_loop`.
3. **RPC health is a type stub.** `ServerEvent::RpcHealth` exists and the
   UI renders it, but nothing on the Rust side pings RPC latency and emits
   it. The UI's "LINK OK/NO LINK" pill only reflects the browser's own
   WebSocket connection to the bot, not per-provider RPC health.
4. **No auth on the control API.** Binds to `127.0.0.1` only, on purpose.
   If you ever need remote access, put it behind Tailscale/SSH tunnel —
   do not change the bind address to `0.0.0.0` without adding auth first.
5. **No test coverage at all.** See "Commands" above for where to start.

## Explicitly out of scope

**Correction to an earlier assumption in this repo's history:** SeaDrop
(OpenSea's own drop protocol) is NOT a walled API — it's open-source,
Spearbit-audited, and deployed as a fixed on-chain singleton
(`0x00005EA00Ac477B1030CE78506496e8C2dE24bf5`). It's fully in scope and
has first-class support via `mint_mode = "seadrop"` and `src/seadrop.rs`.
See README.md's "Mint modes" section.

What's actually out of scope:

- SeaDrop's `mintAllowList`, `mintAllowedTokenHolder`, and `mintSigned`
  stages — each needs external data (a merkle proof, a held gating token,
  a project-issued EIP-712 signature) that isn't obtainable from an RPC
  connection alone. Only `mintPublic` (the no-external-data public stage)
  is implemented.
- Any project that runs a genuinely custom, non-SeaDrop, non-on-chain
  minting flow through its own gated web frontend/API (rare, but exists
  outside SeaDrop-covered OpenSea drops). If asked to script one of these,
  confirm that's really the intent before writing anything — it's a
  different and more adversarial problem than a public contract call.

## Before touching a real mint

1. Confirm the target contract's `mint()` isn't merkle-allowlist gated —
   if it requires a proof argument, sniping fails at the revert regardless
   of code quality. Check the verified source on Etherscan first.
2. Dry-run against a testnet deployment with identical mint-gating logic.
   Don't point this at mainnet untested.
3. Fund wallets from varied sources at varied times — same funding tx,
   same block, same gas price across N wallets is a one-line sybil
   heuristic that can burn allowlist eligibility on the *next* drop from
   the same project even if this mint succeeds.

## Conventions

- Every non-obvious design decision in this repo has a comment explaining
  *why*, not just what — preserve that pattern in new code. A silent
  clever trick is a liability in a codebase that moves real money.
- Prefer explicit failure (return `Result`, log via `bus::log`) over silent
  fallback. A wallet that fails to fire should be loud in the event feed,
  never quietly skipped.
- UI: keep the token system in `ui/src/styles/tokens.css` as the single
  source of color/type/spacing truth. Don't hardcode hex values or px
  sizes in component CSS modules.
