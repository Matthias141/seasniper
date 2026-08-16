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
  executor.rs  — prepare/fire split, ported from a reference sniper's
                 pre-sign pattern (see git history for the audit this was
                 ported from): warm_connections() opens+verifies every RPC
                 endpoint ahead of arm-time; prepare_fire() fetches nonce/
                 gas/chain-id once and signs one raw tx per wallet, with a
                 per-wallet ETH value (0 for free mints, SeaDrop
                 public-stage price otherwise) and jittered gas; fire_
                 prepared() only broadcasts the already-signed bytes over
                 the already-warmed connections — no RPC round trip and no
                 signing left in the fire-time path.
  seadrop.rs   — SeaDrop 1.0 ABI: reads getPublicDrop (price/timing) and
                 builds mintPublic calldata. Public stage only — see its
                 doc comment for why allowlist/signed/token-gated stages
                 aren't (and can't trivially be) covered.
  bus.rs       — ServerEvent enum + broadcast channel, single source of
                 truth for everything the UI displays in real time
  state.rs     — AppState (shared config/wallet-status/armed flag/bus/
                 control channel), ControlMsg enum (Arm/Disarm/FireNow/
                 Prepare — Prepare is internal-only, never sent from
                 api.rs, see control_loop's doc comment in main.rs)
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
**Do not add a second path that calls `executor::prepare_fire` or
`executor::fire_prepared` directly; route everything through
`control_tx`.** This now covers the prepare (pre-sign) phase too, not
just firing — both touch wallet signers, so both have to stay inside
`control_loop`.

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
cargo test                 # calldata encoding + gas jitter math
cargo run                 # starts bot + API on 127.0.0.1:4117

# Frontend
cd ui
npm install
npm run dev                # :5173, proxies /api and /ws to :4117
npm run typecheck
npm run build               # outputs ui/dist/
```

Test suite covers calldata encoding (`encode_mint_calldata`/
`encode_selector_only` in main.rs, `seadrop::encode_mint_public`) and gas
jitter math (`apply_pct_jitter` in executor.rs) — `cargo test`. Encoding
tests assert byte-identical output against independently-built expected
calldata (real mainnet addresses/selectors, cross-checked outside the
crate — see each test's comment), not just "doesn't panic." Jitter tests
cover zero/positive/negative/over-negative boundaries and confirm it
can't produce a negative (wrapped) gas value.

## Known gaps (fix before relying on this for a real mint)

1. ~~Nothing compiled yet.~~ Fixed — see git history. `cargo build` /
   `cargo check` are clean (one pre-existing, expected warning: gap #3
   below, `RpcHealth` unconstructed). If drift resurfaces after a
   dependency bump, same rule as before: fix in place, don't rewrite
   around it unless a function genuinely no longer exists.
2. ~~`mempool_watch` trigger mode was not implemented (silent fallthrough
   to poll_state).~~ Fixed — see git history. `watcher::run_mempool_watcher`
   subscribes via `Provider::subscribe_full_pending_transactions` and
   fires on a pending (pre-confirmation) tx from `mint_enable_admin` to
   the watched contract. **Scope note, not a full implementation of every
   possible interpretation:** matches on (from, to) address pair only, not
   on decoding which function is being called — see the doc comment on
   `run_mempool_watcher` for why (SeaDrop's `updatePublicDrop` is called
   by the nft contract itself, not directly by a human admin, so there's
   no single reliable selector to match on across projects either way).
   Needs a WebSocket RPC with full mempool visibility (Geth 1.11+); many
   free/public RPCs don't expose this even over WS — arming fails loudly
   (auto-disarm + a clear error) rather than silently watching nothing if
   the subscription isn't supported. Any unrecognized `trigger_mode`
   string, or `mempool_watch` with `mint_enable_admin` unset/invalid, also
   now logs an error and falls back to poll_state instead of silently
   doing so with no warning.
3. ~~RPC health was a type stub — nothing pinged it.~~ Fixed — see git
   history. `rpc_health_poll_loop` in main.rs pings every configured HTTP
   RPC every 15s (`eth_blockNumber`, same cadence as `balance_poll_loop`)
   and emits `ServerEvent::RpcHealth` per endpoint with round-trip
   latency. Confirmed (not assumed) that this makes the `RpcHealth`
   unused-variant warning disappear — `cargo build`/`cargo check` produce
   zero warnings as of this fix.
4. **No auth on the control API.** Binds to `127.0.0.1` only, on purpose.
   If you ever need remote access, put it behind Tailscale/SSH tunnel —
   do not change the bind address to `0.0.0.0` without adding auth first.
5. ~~No test coverage at all.~~ Partially fixed — see git history and
   "Commands" above. Calldata encoding and gas jitter math are covered
   (the two things this note originally pointed at as highest
   cost-of-being-wrong). Everything else — watcher trigger logic, the
   prepare/fire split, the control_loop state machine, the API routes —
   still has zero coverage; those need integration-style tests (mock RPC
   or testnet) rather than the pure-function unit tests added here.
6. ~~Pre-signing was fetching gas price once at arm-time and never
   revisiting it.~~ Fixed — see git history. `timestamp` mode still signs
   once, `PREPARE_LEAD_SECS` before the known trigger instant (bounded,
   short window — an explicit, accepted trade-off). `poll_state` mode now
   re-signs every `POLL_STATE_REPREPARE_INTERVAL_SECS` for as long as it's
   armed, since its arm-to-trigger window is unbounded and a single
   prepare at arm time could go stale for minutes or hours.
7. ~~A transaction that lands on-chain but reverts was reported as a
   success.~~ Fixed — see git history. `fire_prepared` now calls
   `.get_receipt()` instead of `.watch()` and checks the receipt's
   `status()` (EVM semantics: `1`/`true` = success, `0`/`false` =
   reverted) before reporting `MintResult { success: true }`. A revert
   reports `success: false` with the tx hash, block number, and gas used
   in `detail` — no revert-reason decoding (would need an extra `eth_call`
   trace replay; out of scope, "false success" was the bug, not missing
   diagnostics). The extra `eth_getTransactionReceipt` call this adds
   happens after `send_raw_transaction` has already dispatched the bytes,
   in the same post-dispatch wait `.watch()` was already doing — nothing
   added to the broadcast/dispatch step itself.
8. ~~poll_state's periodic re-prepare advanced next_nonce on every cycle,
   not just when a batch was actually broadcast.~~ Fixed — found live
   against Sepolia (see step 5's dry-run notes below), not in review.
   `prepare_fire` now only reads `next_nonce`; `main.rs`'s `advance_nonces`
   helper bumps it exactly once, at the point a prepared batch is actually
   committed to firing.
9. ~~Watcher task failures (WS connection/subscribe errors) were
   completely silent — the UI just showed ARMED forever with nothing
   running.~~ Fixed — found live (see below). `main.rs`'s
   `spawn_supervised_watcher` wraps every watcher spawn (timestamp mode
   excepted — it makes no RPC connection) and logs + auto-disarms on
   `Err`. This generalizes the safety net mempool_watch already had for
   its own subscribe failure specifically (gap #2 above).
10. **Prepare-failure log messages only showed a truncated error.** Fixed
    alongside #8/#9 — `format!("prepare failed: {e}")` used anyhow's
    default `Display`, which only prints the outermost `.context()` frame.
    Switched to `{e:#}` (anyhow's alternate Display), which joins the full
    context chain, so a real RPC/revert reason isn't hidden behind a
    generic "estimating gas" or similar.

## Testnet dry run (step 5) — what a live run against Sepolia found

A full dry run was done against real Sepolia infra: SeaDrop 1.0's actual
existing singleton (`0x00005EA0...4bf5` — confirmed deployed via direct
`eth_getCode`, not just the README's deployments table), a purpose-deployed
`ERC721SeaDrop` test token with a real (near-zero, not free) mint price, and
a purpose-built minimal contract for exercising poll_state specifically. See
git history around this note for the exact commits.

**What was verified working, independently (not just trusting the bot's own
report):**
- `timestamp` mode: armed → prepared → fired → 3/3 wallets minted
  successfully. Verified via direct `eth_getTransactionReceipt` calls
  (status `0x1`) and `balanceOf`/`totalSupply` reads — not just the bot's
  own event feed.
- The deliberate-revert path: two wallets pushed to their
  `maxTotalMintableByWallet` cap via manual self-mints, then fired again.
  The bot correctly reported `success: false` with block number and gas
  used for both, while the third (under-cap) wallet correctly reported
  `success: true` — verified independently via `eth_getTransactionReceipt`
  (status `0x0` for the two reverts, `0x1` for the success). This is the
  live confirmation of the gap #7 fix above.
- The UI (`npm run dev`), driven with a real headless browser against the
  live running bot for the first time: wallet balances, nonces, and
  armed/standby state rendered correctly and matched `/api/status`
  byte-for-byte. (Two console errors observed were Google Fonts failing to
  load over this environment's network — cosmetic, unrelated to the app.)

**What could NOT be verified in this specific run, and why (environment
limitation, not a code defect):** `poll_state` and `mempool_watch` both use
`ws_rpc_url` via alloy's WS transport, which is hard-compiled against
`webpki-roots` (a fixed public CA bundle — see `alloy-transport-ws`'s
Cargo.toml, `tokio-tungstenite` with the `rustls-tls-webpki-roots` feature).
The sandbox this dry run ran in terminates TLS through a local proxy with
its own CA, which `reqwest` (used for all HTTP RPC calls) was configured to
trust but this hard-coded WS stack structurally cannot be pointed at. Their
underlying wire protocols (`eth_subscribe("newHeads")` and
`eth_subscribe("newPendingTransactions", true)`) were independently
confirmed to work against the same Sepolia RPC via a raw Python WS client
outside the bot, so this is specifically about this Rust dependency's fixed
TLS trust store in this sandboxed network, not a reachability problem with
the RPC or a logic problem in `watcher.rs`. A real deployment on a normal
network without TLS interception would not hit this. It's exactly what
surfaced gap #9 above though: arming poll_state in this environment used to
just hang forever with the UI still saying ARMED; now it fails loudly and
disarms within ~150ms, with the real error in the log.

**A UI display gap noticed, not fixed:** each `WalletStatus.nonce` shown by
`/api/status` and the WS `snapshot` event is set once at process boot (from
`wallet::load_wallets`'s real on-chain read) and never updated again —
`balance_poll_loop`'s `WalletUpdate` event hardcodes `nonce: 0` and nothing
else writes to `state.wallet_status`'s nonce field. This dry run didn't
initially reveal the gap because each test round happened to restart the
bot process (which re-reads the real nonce at boot); within one continuous
session, the UI's nonce display would go stale after minting while the
actual signing logic (which reads `ManagedWallet.next_nonce` in
`control_loop`, a different value) stays correct. Cosmetic, not a
correctness bug — flagged for a future session rather than fixed here.

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
