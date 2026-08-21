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
                 truth for everything the UI displays in real time (and,
                 via audit.rs's subscription, what gets persisted)
  state.rs     — AppState (shared config/wallet-status/armed flag/bus/
                 control channel/api_token), ControlMsg enum (Arm/Disarm/
                 FireNow/Prepare — Prepare is internal-only, never sent
                 from api.rs, see control_loop's doc comment in main.rs)
  auth.rs      — local bearer-token auth: generates/persists the token to
                 .sniper-token on first run, axum middleware that checks
                 it on every route except GET /api/token (see "Security"
                 section below)
  audit.rs     — persistent append-only audit.log (JSON Lines), a bus.rs
                 subscriber that records arm/disarm/fire/config-change/
                 mint-result events — see its doc comment and
                 RUNBOOK.md's post-fire-verification section
  api.rs       — axum router: GET/PUT /api/config, GET /api/status,
                 POST /api/arm|/api/abort|/api/trigger|/api/copymint/fire|
                 /api/target/resolve|/api/target/set|/api/target/search,
                 WS /ws/events, GET /api/token (unauthenticated bootstrap
                 route)
  copymint.rs  — step 6: watches tracked_wallets for SeaDrop mintPublic
                 activity and mints the same collection with our own
                 wallets. SeaDrop-only, always-on independent of
                 trigger_mode/armed state — see its own doc comment and
                 the "copymint" section below for the full design and
                 safety model.
  opensea.rs   — step 8: OpenSea REST API integration (not the SeaDrop
                 contract) — parses a pasted address/URL, resolves an
                 OpenSea collection slug to a contract address + official
                 links (8b), and free-text collection search (8c). See
                 its own doc comment for the API-key reality check and
                 the namesquatting-risk reasoning behind 8c's design.
  target.rs    — step 8b: orchestrates opensea.rs + seadrop.rs into a
                 verified target (getPublicDrop + a fee_recipient
                 allow-check) — the shared logic behind both
                 /api/target/resolve (read-only) and /api/target/set
                 (which re-runs it fresh rather than trusting an earlier
                 /resolve result).

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
`control_loop`. `ControlMsg::FireCopymint` (step 6) follows this same
rule for a dynamically-discovered mint target: `copymint.rs` and
`api.rs`'s `/api/copymint/fire` route both only ever *send* it, never
touch wallets directly themselves.

**`contract`/`admin_watch_target`/`mint_calldata`/`mint_value` are
runtime-mutable, not fixed at startup (as of step 8a/8b).** Originally
these were `control_loop` function parameters, set once in `main()` from
the config that existed at boot and never revisited. Two changes broke
that assumption on purpose:
- 8a: `mint_value` is re-derived from a fresh `getPublicDrop` call on
  every `ControlMsg::Prepare` outside `timestamp` mode (SeaDrop's
  `updatePublicDrop` can change `mintPrice` — including free → paid —
  at any time between arm and fire).
- 8b: `ControlMsg::SetTarget` (sent only by `api.rs`'s
  `/api/target/set`, itself always re-verifying fresh — see `target.rs`)
  updates `admin_watch_target`/`mint_calldata`/`mint_value` when the
  operator swaps the active `nft_contract` from the UI. `contract` (the
  SeaDrop singleton address) is the one exception that stays fixed —
  8b only swaps the `nftContract` argument within singleton calls, never
  the singleton itself, and is scoped to `mint_mode = "seadrop"` only.

Both changes shadow the original parameter with a `let mut` at the top
of `control_loop` — search for `// step 8a` / `// step 8b` there before
assuming any of these four values are the boot-time constants they used
to be.

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
can't produce a negative (wrapped) gas value. `config.rs` additionally
covers `Config::validate()` — malformed RPC URLs, empty wallet list,
negative gas knobs, and the timestamp-mode-only `trigger_timestamp_unix`
checks (0 is fine, past is rejected, implausibly-far-future is rejected
to catch a milliseconds-into-seconds mistake) — 19 tests total as of
step 7f.

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
4. ~~No auth on the control API.~~ Fixed — see git history and the
   "Security" section below. Local bearer-token auth on every route
   (`GET /api/token` excepted, since it's how the UI bootstraps the
   token) plus an explicit CORS origin allow-list, replacing the old
   `allow_origin(Any)`. Still binds to `127.0.0.1` only, on purpose — the
   token stops a malicious webpage in the same browser, not a network
   attacker; the bind restriction is still the thing stopping the latter.
   If you ever need remote access, put it behind Tailscale/SSH tunnel —
   do not change the bind address to `0.0.0.0` on the strength of the
   token alone.
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
11. **`poll_state` and `mempool_watch` trigger logic has not been
    fire-tested against a live chain — only the underlying transport has
    been confirmed working outside the bot.** The step 5 dry run verified
    `eth_subscribe("newHeads")` and `eth_subscribe("newPendingTransactions",
    true)` work against a live Sepolia RPC via a raw WS client, but never
    got either watcher connected and running inside the bot itself (see
    "Testnet dry run" section above for why — a sandbox-local TLS issue,
    not a code defect). That means `run_state_poll_watcher`'s block-poll
    loop and `run_mempool_watcher`'s (from, to) pending-tx filter have
    never actually fired a real trigger under real timing and real chain
    conditions, in this dry run or any prior one — everything known about
    their correctness so far is code review and unit-level reasoning, not
    a live result. Needs a live run from an environment without this
    sandbox's WS/TLS limitation before either mode should be trusted for
    a real drop.
12. ~~`ruint` 1.16.0 (transitive, via `alloy-primitives` 0.8.26) had two
    open RustSec advisories: RUSTSEC-2025-0137 and RUSTSEC-2026-0220.~~
    Fixed — see git history and "Alloy 0.9 → 2.4.1 upgrade (step 9)"
    below. `alloy` 2.4.1 pulls in `alloy-primitives` 1.6.1, which permits
    `ruint` ≥1.20.0 (both advisories' fixed version) rather than pinning
    it at 1.16.0. `cargo update -p ruint` picked up 1.20.0, and `cargo
    audit` run directly against the resulting `Cargo.lock` confirms zero
    vulnerabilities — not assumed clean from the version bump alone. The
    `ignore:` suppression in `.github/workflows/ci.yml`'s `cargo audit`
    step has been removed accordingly.

## copymint (step 6)

Watches a list of "tracked" wallets and, when one of them mints from a
SeaDrop public-stage drop, mints the same collection with our own
wallets — the same idea as a Solana copy-trading bot, applied to mints
instead of trades. This is the riskiest feature in this codebase: every
other trigger mode (`poll_state`/`timestamp`/`mempool_watch`) fires on a
contract someone configured in advance — a human looked at it before
arming. Copymint fires on whatever a tracked wallet happens to touch,
which nobody has vetted. Full design, reasoning, and safety model live in
`src/copymint.rs`'s doc comment; summary here.

**Scoped to SeaDrop `mintPublic` only, deliberately** — not "replay
arbitrary calldata to an arbitrary contract with a different signer."
That's dangerous in a non-obvious way: ordinary `mint()` calldata often
bakes the caller's own address in as the recipient, so replaying it
byte-for-byte with our signer would mint the NFT to THEM while WE pay gas
and price. SeaDrop's `mintPublic` sidesteps this entirely —
`minterIfNotPayer = Address::ZERO` always mints to whoever pays (per
`seadrop.rs`'s own doc comment on that field), so this module never
replays the tracked wallet's calldata; it only ever needs to know which
`nftContract`/`feeRecipient` they used, then builds and signs a fresh
call of its own.

**Lifecycle is independent of `trigger_mode`/armed state.** The three
existing watchers target ONE specific configured contract; copymint
watches for tracked-wallet activity against ANY SeaDrop collection, so it
runs for the whole process lifetime once `tracked_wallets` is non-empty
(same pattern as `balance_poll_loop`/`rpc_health_poll_loop`), not gated
by whether the main watcher is armed.

**Detection → verification → free/paid split**, in that order:
1. `copymint.rs`'s watcher subscribes to full pending transactions (same
   `Provider::subscribe_full_pending_transactions` call
   `watcher::run_mempool_watcher` already uses) and filters for: sender
   in `tracked_wallets`, target is the configured SeaDrop singleton, and
   the calldata's selector matches `mintPublic`. On a match, decodes only
   `nftContract`/`feeRecipient` — never `minterIfNotPayer`/`quantity`.
2. Before treating a match as an opportunity at all, calls
   `seadrop::fetch_public_drop` fresh and confirms the drop is actually
   currently live (`startTime <= now <= endTime`). This stands in for the
   human review every other trigger mode gets from being manually
   configured — a tracked wallet's tx could be to an expired,
   malicious-lookalike, or otherwise uninteresting contract, and this is
   what catches that.
3. `ServerEvent::CopyOpportunity` is emitted for BOTH free and paid
   opportunities, whether or not it goes on to auto-fire.
4. Free (`mint_price_wei == 0`, read fresh from `getPublicDrop`, never
   guessed) auto-fires if `copymint_auto_fire_free` (default `true`) —
   downside bounded to gas, already capped by `max_priority_fee_gwei_cap`.
5. Paid opportunities NEVER auto-fire. `copymint.rs`'s `should_auto_fire`
   doesn't even accept `copymint_auto_fire_paid` as a parameter — there is
   no code path in this file that can auto-fire a paid opportunity under
   any config. That flag only controls whether the UI offers/enables a
   manual fire button for a given opportunity
   (`ui/src/components/CopyOpportunities.tsx`); the backend route it hits
   (`POST /api/copymint/fire` in `api.rs`) doesn't read that flag either —
   it fires because a human authenticated and clicked, which is the
   actual manual confirmation this whole split exists to require. That
   route also never trusts a client-echoed price: it re-runs
   `getPublicDrop` fresh and re-checks liveness + `max_copymint_price_wei`
   (default `0` — nothing paid is fireable until explicitly raised)
   immediately before firing.
6. Firing, for both free-auto and paid-manual, routes through a new
   `ControlMsg::FireCopymint { contract, calldata, value }` sent to the
   same single-writer `control_loop` every other action uses — JIT-signed
   via the same prepare→fire fallback path `FireNow` already uses when
   nothing was pre-signed (there's no "prepare ahead of time" for
   copymint; the target isn't known until the moment of detection).
   Deliberately does NOT touch `armed`/the main watcher's state — a
   copymint fire can happen while the main watcher is armed, disarmed, or
   not configured at all this run.

**Safety properties traced and tested explicitly (6e), not just
asserted** — see `src/copymint.rs`'s test module:
- `copymint_never_propagates_tracked_wallets_own_minter_choice`: builds a
  tracked wallet's calldata with the tracked wallet's OWN address as
  `minterIfNotPayer` (the adversarial case this whole design exists to
  prevent), decodes it, rebuilds our own call from the decoded values via
  `seadrop::encode_mint_public`, and asserts the resulting
  `minterIfNotPayer` is `Address::ZERO` — never the tracked wallet's
  address. A real, executable proof of property #1, not a comment.
- `paid_opportunities_never_auto_fire_regardless_of_config`: asserts
  `should_auto_fire(false, true)` and `should_auto_fire(false, false)`
  are both `false` — property #2, made testable by the fact that the
  function's signature structurally cannot express "auto-fire a paid
  opportunity" at all (it has no parameter for
  `copymint_auto_fire_paid`).

**KNOWN LIMITATION — inherits gap #11 in full, does not close it.**
Copymint's entire detection mechanism is
`subscribe_full_pending_transactions`, the exact same call gap #11 flags
as never having been successfully fire-tested end-to-end against a live
chain from this sandbox (alloy's WS transport fails TLS validation
against this sandbox's intercepting proxy — structural, confirmed as
recently as step 9e). Copymint's parsing/filtering/decoding/firing-
decision logic is unit-tested against synthetic pending-tx-shaped data
(see `src/copymint.rs`'s tests), but has NOT been live-fired here. This
feature landed after step 5's original testnet plan was written and
after step 9e's dry run, so it carries its own, so-far-unmet validation
requirement on top of gap #11's existing one: **needs real testnet
validation — tracked-wallet detection actually firing, an opportunity
actually surfacing in the UI, manual fire actually working — from an
environment without this sandbox's WS/TLS limitation before it's
trustworthy for a real drop.**

## Target resolution (step 8b/8c)

Lets the operator point the bot at a SeaDrop collection from the UI —
paste an address/URL and confirm (8b), or search by name first (8c) —
instead of hand-editing `nft_contract` in `config.toml`. Full design
lives in `src/opensea.rs`/`src/target.rs`'s doc comments; summary here.

**Two-step shape, same as copymint's**: resolving/searching never
changes bot state. Only `/api/target/set` commits, and it always
re-verifies fresh (never trusts an earlier `/resolve` result or a
client-echoed price) before sending `ControlMsg::SetTarget` through
`control_loop` and persisting to `config.toml`.

**8b vs 8c — deliberately different risk postures, not the same feature
with an extra search box bolted on:**
- 8b (paste an address or OpenSea URL): the operator already has one
  specific thing in hand. A raw address needs zero external calls; an
  OpenSea URL needs `opensea_api_key_env` configured (see the API-key
  correction below) to resolve the slug to a contract address.
- 8c (search by name): returns AMBIGUOUS candidates — this is
  meaningfully riskier, because namesquatting (a fake collection
  deployed with an identical or near-identical name to catch people
  searching for a real drop) is a standard scam pattern in this space,
  not a hypothetical. Designed around explicitly:
  - `opensea::search_collections` never auto-selects a result; the UI
    (`TargetResolver.tsx`'s "SEARCH BY NAME" section) renders a picklist,
    with a visible, deliberately louder-than-routine warning that results
    are unverified. OpenSea's result ordering is never treated as a
    trust signal anywhere in this codebase.
  - **No Twitter/X search or scraping integration exists, and none
    should be added.** Real-time X search needs a paid API tier,
    scraping violates X's ToS, and — the more fundamental reason — X is
    itself a common vector for spreading fake mint links, so using it as
    an input to a system that decides where to send money is backwards.
    Instead: `resolve_slug` surfaces whatever official links (X, Discord,
    website, etc.) a project put on their OWN OpenSea collection page,
    so the operator can go verify on X themselves, using their own
    judgment — those links are a pointer, never treated as verified.
  - Picking a search result does NOT skip straight to `/api/target/set`
    — it feeds the candidate's slug back into the exact same
    resolve-then-verify pipeline 8b's paste box uses (see
    `TargetResolver.tsx`'s `pickSearchResult`). A search hit is not
    pre-verified just because it came from OpenSea's index; every
    candidate still needs a real `getPublicDrop` call before it means
    anything.

**API key reality check, found by checking rather than assuming.** This
feature's original framing was "8b's slug/URL resolution needs no API
key" — that's out of date. Confirmed directly against docs.opensea.io:
OpenSea's v2 `/collections/{slug}` endpoint (8b) and `/search` (8c) both
now require an `x-api-key` header. Only a raw `0x` address (8b) needs
zero external calls. Also confirmed directly, not assumed still the old
multi-day-manual-only process: OpenSea now also offers an instant
self-serve key (one API call, no signup) alongside the traditional
application-form key — but the instant key expires after 7 days, so it's
not a set-once-and-forget credential (see `opensea_api_key_env`'s doc
comment in `config.rs`).

**Scope**: seadrop mode only, same as copymint — no `getPublicDrop`/
collection concept exists for `mint_mode = "custom"`. **Not step-up-auth
gated yet**: `/api/target/set` changes where the bot's next mint sends
money, the same sensitivity class as `/api/arm`/`/api/trigger`, but step
10 (identity) isn't merged as of this writing — there's a TODO comment
directly on that route so this isn't missed when 10f lands.

## Alloy 0.9 → 2.4.1 upgrade (step 9)

Done specifically to clear gap #12 above (the two `ruint` advisories,
whose only real fix was this major-version jump) — not a routine bump,
treated as its own dedicated, careful piece of work per this repo's own
"Status" section warning that alloy's surface shifts even across minor
versions. See git history for the individual 9a-9g commits.

**Scoped first (9a), before touching code.** Read alloy's actual v1.0
migration guide and docs.rs for 2.4.1, checked every alloy API this
codebase calls. Two confirmed hit sites going in: `FunctionExt::
abi_decode_output` losing its `validate: bool` parameter (hits
`seadrop.rs` directly), and the `ReqwestProvider` type alias being
removed (hits `executor.rs`). Everything else was either confirmed
unaffected or explicitly flagged unknown-pending-compile, not assumed.

**Compiled and fixed mechanically (9b)** — see `src/*.rs` and git
history for the full file-by-file diff. Two changes were judgment calls,
not mechanical renames:
- Every `ProviderBuilder::new()` call site (8 total) now chains
  `.disable_recommended_fillers()`. Alloy 2.x attaches a default gas/
  nonce/chain-id filler stack by default now; this codebase's entire
  prepare/fire split exists specifically to avoid exactly that kind of
  implicit RPC-time filling (see `executor.rs`'s own comment on why
  manual `gas_limit` became load-bearing). This bot never calls
  `.send_transaction()` (signs manually, calls `.send_raw_transaction()`)
  so the filler stack likely never engaged either way, but disabling it
  restores exact 0.9.x bare-provider semantics and keeps `HttpProvider =
  RootProvider` a simple type instead of the default's verbose
  `FillProvider<JoinFill<...>, RootProvider>`.
- `watcher.rs`'s `tx.from == admin` (load-bearing for `mempool_watch`'s
  admin-address match) became `tx.inner.signer() == admin` — `Transaction`
  no longer has a direct `from` field (compiler-confirmed field list:
  `inner`, `block_hash`, `block_number`, `transaction_index`,
  `effective_gas_price`, `block_timestamp`). Inferred the accessor from
  `tx.inner.tx_hash()` already working elsewhere in the same file, then
  confirmed against the compiler rather than guessing from memory.

**Re-verified `seadrop.rs`'s `getPublicDrop` decode live (9c)**, same
dual-decoder rigor as step 2's original verification — a wrong decode
here fails silently as a wrong price/timestamp, not a compile error, so
a clean `cargo build` was explicitly not treated as sufficient evidence.
Called `getPublicDrop` on the real SeaDrop 1.0 mainnet singleton for
EVERYBODYS (the same real collection step 2 used) through the upgraded
alloy, from a standalone scratch binary. Decoded the raw response via
the actual post-upgrade call shape (`abi_decode_output(&raw)`, no
`validate` arg) AND an independent hand-written raw-byte decoder with
zero alloy decode logic involved. Both agree, and both match step 2's
original values byte-for-byte: `mintPrice=36000000000000000 wei`,
`startTime=1666908000`, `endTime=1669590000`, `maxTotalMintableByWallet=
20`, `feeBps=750`, `restrictFeeRecipients=true`.

**Re-read the test assertions, not just pass/fail (9d).** The calldata
encoding tests (`seadrop.rs`, `main.rs`) route through `Function::parse`,
`DynSolType::coerce_str`, `abi_encode_input`, `Function::selector` — none
of which required a single edit during 9b's compile-fix pass, meaning
these tests still exercise the identical code path they always did, not
a coincidentally-passing different one. `executor.rs`'s gas-jitter tests
are pure `u128`/`i128` arithmetic, not applicable to this upgrade at all.

**Pubsub / gap #11 (9e) — NOT closed, confirmed not closable in this
environment, not just assumed still blocked.** Freshly re-tested (this
session's container had no leftover `.testnet-keys/` or `forge`/`cast`
install from step 5's — a different container instance): a raw Python
`websockets` client reached `wss://ethereum-sepolia-rpc.publicnode.com`
and received a live block header with no issue. Alloy 2.4.1's own WS
transport, hitting the identical URL from a standalone scratch binary,
failed with `invalid peer certificate: UnknownIssuer` — the exact same
failure mode as step 5, for the exact same reason (alloy's WS stack is
hard-compiled against `webpki-roots`, which structurally can't be pointed
at this sandbox's TLS-interception proxy the way `reqwest`/HTTP can).
Gap #11's wording is unchanged, not because it wasn't reconsidered but
because this result is more evidence for the same conclusion, not new
information. Confirmed separately: `subscribe_full_pending_transactions`
itself required zero changes during the upgrade (only `.connect_ws()`'s
name earlier in the same chain changed) — its signature and behavior are
identical to before.

**Close-out (9f):** `cargo build --all-targets` / `cargo test` (19/19) /
`cargo clippy --all-targets -- -D warnings` all clean throughout.
`cargo audit` confirmed clean directly (zero vulnerabilities; two
pre-existing, unrelated "unmaintained" informational warnings remain —
`derivative`, `paste` — not gated, not new). Gap #12 marked resolved
above; `.github/workflows/ci.yml`'s `ignore:` suppression removed.

## Security (step 7)

Step 7 (7a-7g) closed the gap the rest of this file's feature work had
been assuming was sound without anyone actually checking: this bot holds
private keys and moves real money, and until step 7 nobody had audited
its own key handling, hardened its control API, wired up CI, written a
release process, or given an operator anything to do when something goes
wrong. Summary below; see git history for the individual 7a-7g commits
and `RUNBOOK.md` for the operational playbooks this section backs.

**7a — key-handling self-audit (findings: clean).** Grepped `wallet.rs`,
`executor.rs`, and `main.rs`'s `control_loop` for `PrivateKeySigner`/raw
key strings reaching any logging call — none found. Alloy's signer types
(`PrivateKeySigner`/`LocalSigner<C>`) don't derive `Debug`, so an
accidental `{:?}` on one is a compile error, not a runtime leak — a
structural safety net, not just discipline. `.gitignore` was verified
directly (`git check-ignore`), not assumed, and covers `config.toml`,
`.env`, `.sniper-token`, `.testnet-keys/`, and (as of 7g) `audit.log`.
`PUT /api/config`'s full type definition and handler were read end to
end: `Config`/`WalletCfg` only ever carry `private_key_env` (an env var
*name*), never a key value — round-tripping the API cannot leak key
material because the type has no field for it to leak through.

**7b — control API hardening: CORS + local bearer-token auth.** Two
independent fixes, both in `src/auth.rs`, `src/api.rs`, `state.rs`, and
mirrored on the UI side (`ui/src/lib/api.ts`, `useEventSocket.ts`,
`App.tsx`):
- **CORS** went from `allow_origin(Any)` to an explicit allow-list
  (`http://localhost:5173` dev, `http://127.0.0.1:4117` /
  `http://localhost:4117` prod). No wildcard fallback.
- **Every route requires a local bearer token** (`Authorization: Bearer
  <token>` for HTTP; `?token=` query param specifically for the
  `/ws/events` WebSocket upgrade, since browsers cannot set a custom
  header on a WebSocket handshake) except `GET /api/token`, the
  unauthenticated bootstrap route the UI calls once at startup
  (`initAuth()`) to learn the token in the first place. The token itself:
  32 random bytes hex-encoded, generated on first run, persisted to
  `.sniper-token` (gitignored, `chmod 600` on Unix), read from disk on
  every subsequent boot rather than regenerated.

**What this model protects against:** binding to `127.0.0.1` already
stops anything off-machine; the token additionally stops a malicious
webpage open in the *same browser* — a bad ad, a compromised site, a
rogue extension content-script — from silently `fetch()`-ing or opening a
raw WebSocket to arm/fire/reconfigure the bot. This is a real, known
attack class against unauthenticated local APIs (DNS rebinding,
localhost-CSRF), not a hypothetical.

**What it does NOT protect against, precisely:** anything that already
has filesystem access to `.sniper-token` — native malware running as the
same OS user, a compromised browser extension with broad host/file
permissions, another process on a shared machine that can read your
files. Reading the token file directly is exactly as good as stealing it
over HTTP; this is a local-agent auth model (stops arbitrary web content
from reaching the API), not a defense for a fully compromised machine.
Full precision on this trade-off lives in `ui/README.md`'s security
section — read that before assuming the token means more than it does.

**7c-7g in brief** (each has its own detailed commit message and, where
applicable, a doc comment at the point of implementation):
- **7c**: `.github/workflows/ci.yml` — build/test/clippy/`cargo audit` on
  the Rust side, typecheck/build/`npm audit` on the UI side, gitleaks
  secret-scanning (full history, `fetch-depth: 0`) on every push. Fixed
  two pre-existing gaps this surfaced (a clippy `too_many_arguments` trip
  in `control_loop`, a missing `ui/src/vite-env.d.ts`) so the pipeline
  isn't red from its first run. **Branch protection is not enabled** —
  no branch-protection/ruleset API was available to check or set it from
  this session, and it needs manual verification/enabling by someone
  with repo-settings access; this workflow is not yet an enforced merge
  gate.
- **7d**: `.github/workflows/release.yml` — tag-triggered (`v*`), clean
  checkout, `cargo build --release` + `npm run build`, packaged together
  (binary + `ui/dist` + `config.example.toml` + `README.md` +
  `RUNBOOK.md`) into one tarball attached to a GitHub Release. No
  deployment automation beyond that.
- **7e**: `RUNBOOK.md` — checklists (not prose) for suspected key
  compromise, post-fire verification, drained wallets, and API token
  compromise.
- **7f**: `Config::validate()` in `config.rs`, called from both
  `Config::load` (startup) and `api::put_config` (every UI save) —
  malformed RPC URLs, empty wallet list, negative gas values, and (in
  `trigger_mode = "timestamp"`) an implausible `trigger_timestamp_unix`
  all get rejected with a specific reason instead of silently written.
- **7g**: `audit.rs` — a `bus.rs` subscriber that persists arm/disarm/
  fire/config-change/mint-result events to a gitignored, append-only
  `audit.log` (JSON Lines), giving `RUNBOOK.md`'s "confirm what happened"
  guidance something durable to check beyond `bus.rs`'s ephemeral
  256-event buffer or a terminal that may no longer be open.

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
limitation on the connection, NOT proof the watcher logic itself is
sound — see below for why those are different claims):** `poll_state` and
`mempool_watch` both use `ws_rpc_url` via alloy's WS transport, which is
hard-compiled against `webpki-roots` (a fixed public CA bundle — see
`alloy-transport-ws`'s Cargo.toml, `tokio-tungstenite` with the
`rustls-tls-webpki-roots` feature). The sandbox this dry run ran in
terminates TLS through a local proxy with its own CA, which `reqwest`
(used for all HTTP RPC calls) was configured to trust but this hard-coded
WS stack structurally cannot be pointed at. A real deployment on a normal
network without TLS interception would not hit this specific connection
failure. It's exactly what surfaced gap #9 above though: arming
poll_state in this environment used to just hang forever with the UI
still saying ARMED; now it fails loudly and disarms within ~150ms, with
the real error in the log.

**Be precise about what this does and doesn't prove.** The underlying
wire protocols (`eth_subscribe("newHeads")` and
`eth_subscribe("newPendingTransactions", true)`) were independently
confirmed to work against the same Sepolia RPC via a raw Python WS client
outside the bot — that's real evidence the *transport* is reachable and
the RPC supports both subscription types. It is NOT evidence that
`run_state_poll_watcher`'s block-poll loop or `run_mempool_watcher`'s
(from, to) pending-tx filter behave correctly under real timing and real
chain conditions — neither watcher has ever actually connected and fired
against a live chain, in this dry run or any prior one. Every review of
that logic so far (steps 1-4, and the "what could not be verified" note
above) is code review and unit-level reasoning, not a live result. Don't
read the transport check as having verified more than it did — see gap
#11 below.

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

## Identity (step 10)

Adds real operator identity in front of the control API, replacing the
step 7 bearer-token-only model where anyone with `.sniper-token` (or
network access to it, if that model's own precise threat boundary is
ever misjudged) had full control. Google Sign-In (10c) establishes who
you are; TOTP (10d) and WebAuthn/passkey (10e) each add a second factor,
required together before a session reaches `admin_tier`; step-up auth
(10f) then layers a short-lived, per-request freshness re-check on top
of an `admin_tier` session for the money-moving routes specifically
(arm/trigger/config/target-set). `src/identity/*.rs` and
`migrations/000{1,2,3}_*.sql` hold the implementation; `src/auth.rs`'s
`require_token_or_session`/`require_step_up` are where the control API
actually enforces it. Bearer-token-only mode (step 7) still works
unchanged when identity isn't configured for a given instance — see
`AuthGate` in `ui/src/App.tsx`.

**10i — security self-audit, findings below (all clean; nothing here
required a code change).** Scope, per this step's own purpose as the
trust-check on everything 10a-10h built: secrets never reaching a log
or the durable audit trail, cookie hardening, and `.gitignore` coverage
for every new secret-bearing file this step introduced.

- **`.gitignore` coverage, verified with `git check-ignore -v`, not
  just read from the file:** `identity.db`, `identity.db-wal`,
  `identity.db-shm`, and `.session-key` are all genuinely ignored.
- **Secrets never reach `bus::log`/`audit.log`.** Grepped every
  `bus::log` call site touching the identity module: none embed a raw
  TOTP code, TOTP secret, session id, cookie signing key, or WebAuthn
  assertion — only structural `anyhow` context strings (`{e:#}`),
  session/device row ids (opaque, not secrets), and one user-visible
  Google account email (`"google sign-in succeeded for {email}"`).
  That email only reaches already-authenticated admin clients over
  `/ws/events` (itself gated by `require_token_or_session`) — it's
  never persisted, since `audit.rs`'s bus subscriber explicitly skips
  `ServerEvent::Log` (see its own comment: "routine and/or high-volume
  — not what RUNBOOK.md's checklists need a durable record of").
  `src/identity/oidc.rs` and `src/identity/webauthn.rs` have zero
  logging call sites at all. No request-body-logging middleware exists
  anywhere in `main.rs`/`api.rs` that could accidentally capture a
  POSTed TOTP code or WebAuthn ceremony payload.
- **Cookies (`sniper_session`, `sniper_oidc_flow`), read directly from
  the `Cookie::build` call sites in `api.rs`:** both set
  `.http_only(true)`, `.secure(true)`, `.same_site(SameSite::Lax)`, and
  are carried through axum-extra's `PrivateCookieJar` (encrypted +
  signed, not just signed) — the raw session id is never visible
  client-side even via devtools. `sniper_oidc_flow` is additionally
  scoped to `.path("/auth/google")`, narrower than the session cookie's
  `.path("/")`, matching its short-lived, single-purpose role.
- **Key material (`.session-key`, `identity/crypto.rs`):** 96 random
  bytes generated with `rand::thread_rng()` on first run, `chmod 600`
  on Unix (same pattern `auth::load_or_create_token` already used for
  `.sniper-token`), split into the cookie-jar key (64 bytes) and an
  AES-256-GCM key for TOTP secrets at rest (32 bytes) — one file rather
  than two, since both have the same "leaks it → forge a session or
  decrypt a TOTP secret" blast radius (see the file's own doc comment).
  TOTP ciphertext uses a fresh random nonce per encryption, verified by
  `crypto.rs`'s own test that ciphertext never equals plaintext and
  that a wrong key fails to decrypt.
- **DB schema (`migrations/0001_identity.sql`) has no plaintext secret
  columns.** `totp_secrets.secret_ciphertext`/`secret_nonce` are the
  AES-256-GCM output above, never the raw secret.
  `webauthn_credentials.passkey_json` is `webauthn_rs`'s public-key
  credential data — WebAuthn private keys never leave the
  authenticator by design, so this isn't secret material to begin
  with. `sessions.id` is the only thing that ever goes in the
  encrypted cookie (per the migration's own comment) — no separate
  session-secret column exists to leak.
- **One informational, non-blocking observation, not a finding:** TOTP
  setup (`post_totp_setup_start`/`post_totp_setup_verify`) and WebAuthn
  registration are gated by `require_session` (an active session)
  rather than `require_step_up`. This is correct by the design already
  documented in `session.rs`'s doc comment — a session mid-chain
  (Google done, TOTP/WebAuthn still pending) has to be able to reach
  these routes to ever complete the chain at all, and step-up auth is
  explicitly a separate, later concept layered on top of an
  already-`admin_tier` session, not a substitute for the login chain
  itself.

**10j / wrap-up.** A final pass over step 10 as a whole (not just 10i's
narrower secrets/cookies/gitignore scope) looking for anything left
inconsistent across the sub-steps — found one: `state.rs`'s doc comment
on `ControlMsg::SetTarget` still said `/api/target/set` was "not
step-up-auth-gated yet... because step 10 isn't merged," a leftover
from when that comment was written (step 8b) predicting 10f. 10f had
already fixed the actual gap — `post_target_set` in api.rs calls
`require_step_up` first thing, and api.rs's own comment already says
so correctly — but the state.rs side was never updated to match, so it
sat there actively misleading (falsely implying an unfixed security
gap) for as long as 10f-10i took to land. Checked every other route
this codebase treats as money-adjacent (`/api/arm`, `/api/trigger`,
`/api/copymint/fire`, `PUT /api/config`, `/api/target/set`) against
`require_step_up`'s actual call sites, not against their comments —
all five call it. No other stale TODO/FIXME exists anywhere in `src/`
or `ui/src/` as of this pass (`grep -rn "TODO\|FIXME\|XXX"` — the two
remaining hits are the now-accurate api.rs/state.rs comments
themselves, cross-referencing each other). Step 10 (10a-10j) is
complete.

## Cloudflare Tunnel + Access (step 10.5)

Adds phone reachability with no app install (vs. Tailscale, which needs
the Tailscale app on the phone) — an OUTER layer in front of step 10's
own auth, not a replacement for it. Full setup and the precise
"what Access does/doesn't protect against" boundary live in
`ui/README.md`'s "Reaching this from your phone" section;
`RUNBOOK.md` §6 covers a compromised Access policy. Summary here.

**10.5a — one canonical origin, not two.** Considered treating Tailscale
and Cloudflare as separate live origins (separate passkey registrations
per origin), rejected: `trg_webauthn_admin_cap` caps at 2 admin-tier
WebAuthn credentials **per user**, with no per-origin dimension in the
schema at all — a single laptop registering against two origins alone
would burn both slots, leaving zero room for a phone. Chose instead:
`Config::google_oauth_redirect_url`'s host is the ONE origin for
everything — Google's OAuth redirect target, the Cloudflare Tunnel's
public hostname, AND WebAuthn's rp_origin, all derived from the same
value (see `identity::webauthn::derive_origin`, extracted specifically
so CORS and WebAuthn can never independently drift on what "our origin"
means — used by both `WebauthnState::new` and `api::router`'s CORS
allow-list). Switching from a Tailscale hostname to a public Cloudflare
one is a real cutover, not an additive change: every existing passkey
is origin-bound and stops validating the moment the origin changes —
budget for re-registering devices right after, not mid-incident.

**10.5b — the bind address does not change.** `API_BIND_ADDR` in
main.rs stays `127.0.0.1:4117`; `cloudflared` runs as a local client
that reaches IN to that address over an outbound connection to
Cloudflare's edge, same shape as step 7b's original bind-to-localhost
reasoning — a token/session model narrows who can act once a request
arrives, it was never a substitute for not exposing the port directly,
and that's still true here. `api::router` now takes
`google_oauth_redirect_url` as an explicit parameter (rather than
reading `state.config` internally) because CORS origins are a
decided-once-at-boot property, same shape as `auth::require_token_or_
session`'s own mode choice — see the function's doc comment. Covered by
real request-level tests (`api.rs`'s `cors_tests` module) asserting the
actual `access-control-allow-origin` response header, not just that the
code compiles: the configured origin is echoed back, an unrelated
origin is rejected even with one configured, an unconfigured
`google_oauth_redirect_url` doesn't silently allow anything, and the
existing step 7b dev/prod origins keep working unchanged alongside it.

**10.5c — Access reuses step 10c's Google OAuth setup for its own login
gate** (rather than email-OTP as a second, independent identity system
to keep in sync) and is scoped to an Access **group**, not a flat email
list, so step 11 can add operators to it at invite time without
touching the Access policy itself. Stated precisely, because this is
easy to overstate: Access blocks unauthenticated traffic at Cloudflare's
edge (real attack-surface reduction — none of that traffic reaches this
process at all) but has no knowledge of `admin_tier`/step-up state;
passing Access gets you exactly as far as step 10's own login wall, not
one step further. **A compromised or over-broad Access policy is a
reduced-attack-surface incident, not automatically an
arm/fire-capable-attacker incident** — RUNBOOK.md §6's first move is
always to cross-check step 10's own audit trail/`sessions` table before
assuming the worst.

## Robinhood Chain support (step 13)

Adds Robinhood Chain (Arbitrum Orbit, EVM-compatible, gas paid in ETH —
mainnet chain id 4663, testnet 46630) as a supported network, plus
real, chain-agnostic fire-path timing instrumentation across every
network this bot targets. Combined because the timing work is what
turns "Robinhood Chain is supported" into a checked claim against
MintDash's own published numbers, not just "it compiles."

**13a — SeaDrop's singleton confirmed live, not assumed from
morsyxbt/nft-public-mint's chain list.** Real `eth_getCode` calls
against `0x00005EA00Ac477B1030CE78506496e8C2dE24bf5` on both Robinhood
mainnet and testnet returned real bytecode, byte-identical to Ethereum
mainnet's own deployment except for one ~20-byte segment — a
per-chain-id EIP-712 domain-separator immutable, confirmed by locating
its literal chain-id hex (`1237`/`b626`) at the exact diff offset, not
a sign of a different or tampered deployment. Full detail in
`seadrop.rs`'s doc comment.

**13b — no config schema changes needed.** `chain_id` was already read
live per-instance (`executor.rs`'s `get_chain_id()` call, confirmed by
reading the code, not assumed) rather than hardcoded — the exact
footgun this step set out to check for doesn't exist here.
`ws_rpc_url`/`http_rpc_urls`/`seadrop_address` were already free-text
and overridable. Added a Robinhood Chain example to
`config.example.toml` with confirmed Alchemy endpoints
(`robinhood-mainnet/testnet.g.alchemy.com` — Alchemy's own docs
confirm support, checked directly).

**13c — block-time scaling, examined and decided, not silently
ignored.** `run_state_poll_watcher` does one `eth_call` per block;
Robinhood Chain's ~100ms blocks mean roughly 120x the call volume of
mainnet's ~12s blocks. Decision: don't throttle it — a per-block check
is exactly correct sniper behavior, and an artificial delay would work
against the tool's whole purpose. The real mitigation is an
adequately-provisioned RPC plan for a fast chain, not a code change —
see `watcher.rs`'s doc comment for the full reasoning. **13d could not
directly measure the per-block call rate this predicts** (see below) —
the reasoning is confirmed sound, the specific number is not yet
independently observed.

**13d — live dry run against Robinhood Chain testnet, from real funded
wallets (`.testnet-keys/wallet1`), not simulated:**
- Found a genuinely live, currently-open free-mint SeaDrop collection
  on testnet by reading real successful `mintPublic` transactions
  against the singleton (`nftContract =
  0xc4A245473372AD4c83DA323791A8815957A94b70`), rather than deploying
  a fresh test token — a real target was already there.
- **timestamp mode: fully live-fired and independently verified.**
  Armed → prepared → fired → confirmed, then checked directly via
  `eth_getTransactionReceipt` (status `0x1`) and the receipt's own
  ERC721 `Transfer` log (from the zero address to our wallet, tokenId
  3) — not just trusting the bot's own report, same bar step 5's
  original Sepolia dry run set. Real numbers, this specific attempt
  (n=1, not a distribution): `send_to_ack_ms = 236`,
  `dispatch_to_inclusion_ms = 7551`.
- **poll_state mode: armed, and failed exactly the way gap #11
  predicted — confirmed, not assumed to still apply.** The watcher's
  WS connection failed with `invalid peer certificate: UnknownIssuer`,
  the identical error signature step 9e's Sepolia dry run hit against
  a completely different RPC endpoint — definitive evidence this is
  this sandbox's TLS-interception limitation (alloy's WS transport
  hard-compiled against `webpki-roots`), not anything specific to
  Robinhood Chain. The safety net from gap #9 worked correctly: failed
  loudly and auto-disarmed within about a second (confirmed via
  `audit.log`'s arm/disarm timestamps one second apart), not a silent
  hang. This means 13c's ~10-calls/sec prediction remains reasoned
  analysis, not a directly observed number — poll_state has never
  successfully connected on ANY chain from this specific sandbox, gap
  #11's scope, unchanged by this step.
- mempool_watch was not attempted separately — it shares poll_state's
  exact WS-connection dependency and would fail identically.

**13e — real timing instrumentation, chain-agnostic by construction.**
`ServerEvent::MintResult` now carries `trigger_to_dispatch_ms`,
`send_to_ack_ms`, `dispatch_to_inclusion_ms`, and `prepare_age_ms` —
plain `Instant` deltas in `executor.rs` with zero chain-specific
logic, persisted in `audit.log` and shown in the UI's event feed per
attempt. This is what made 13d's numbers above possible to capture at
all, and what a future mainnet or Sepolia run would produce directly
comparable numbers from too.

**13f — honest comparison against MintDash's published Robinhood Chain
figures (send→ack p50 117ms, mintDuration p50 136ms):**

| Metric | This run (n=1) | MintDash (p50) |
|---|---|---|
| send→ack | 236ms | 117ms |
| dispatch→inclusion | 7551ms | 136ms |

**Read this precisely, not as "we're slow":**
- **n=1 vs. p50.** MintDash's numbers are a distribution's median over
  presumably many attempts; this is one single attempt. A single data
  point isn't comparable to a p50 the same way two p50s would be —
  this table is a starting reference point, not a verdict.
- **send→ack (236ms vs 117ms) is a fair-ish comparison** — same
  concept (RPC accepting the broadcast), and this bot's number came
  from Robinhood's own public testnet RPC from a cloud sandbox with no
  particular network proximity to it, not a colocated or
  low-latency-optimized path. MintDash explicitly runs their own
  colocated node (per this task's own framing) — a ~2x gap between an
  arbitrary public RPC and a colocated node's own accept latency is
  plausible and not alarming on its face.
- **dispatch→inclusion (7551ms vs 136ms) is NOT an apples-to-apples
  comparison and shouldn't be read as "56x slower."** Robinhood
  Chain's ~100ms blocks mean 136ms is roughly one block's worth of
  time — MintDash's `mintDuration` most likely measures actual
  on-chain inclusion latency at the protocol level. This bot's
  `dispatch_to_inclusion_ms` measures wall-clock time until
  `get_receipt()`'s own internal polling loop (alloy's default
  poll-and-retry implementation, not a push-based subscription) notices
  the receipt — that polling interval, not raw chain inclusion speed,
  plausibly dominates a 7.5-second result on a 100ms-block chain. This
  number reflects THIS bot's current receipt-detection method more
  than it reflects Robinhood Chain's real inclusion speed, and isn't
  safe to read as a direct chain-speed comparison without first
  confirming (not yet done) whether switching `fire_prepared` to a
  push-based subscription for inclusion detection meaningfully closes
  this gap on a fast-block chain specifically. Flagged as a real,
  concrete next step rather than treated as settled — the honest
  conclusion of 13f is "we don't yet know how close we are on
  inclusion speed," not "we're roughly on par" or "we're far behind."

**SUPERSEDED by step 14 (kept here, not deleted, so the before/after
improvement stays visible).** Step 14 fixed the exact mechanism this
section flagged as the open question (`get_receipt()`'s poll-interval
artifact) and re-measured with a real n=15 distribution on both
Robinhood Chain testnet and a Sepolia baseline. See "Fire-path
inclusion detection + real benchmark (step 14)" below for the current
numbers — the 236ms/7551ms single-attempt figures above are step 13d's
original, now-historical result, left in place as the "before" side of
that comparison.

## Fire-path inclusion detection + real benchmark (step 14)

Fixes the exact gap step 13f left open (`get_receipt()`'s internal poll
interval, not real chain speed, dominating `dispatch_to_inclusion_ms`)
and replaces 13f's single-attempt figures with a real n=15 distribution
on two chains, benchmarked under the same methodology.

### 14a — push-based inclusion detection

New module `src/inclusion.rs`, deliberately separate from
`watcher::run_state_poll_watcher` (step 13c) — that loop asks "has this
drop's mint state flipped active" once per block, BEFORE dispatch;
`inclusion.rs` asks "has THIS already-broadcast tx hash been included"
once per block, AFTER dispatch. Different question, different timing,
different caller — kept in separate files specifically so 13c's
"don't throttle" reasoning (which is about the FIRST loop) is never
mistakenly applied to or read as touching the second.

**Two paths:** PUSH (primary) reuses `watcher.rs`'s own
`subscribe_blocks()` call — one shared WS subscription per
`fire_prepared` batch (not one per wallet, avoiding N redundant WS
connections for N wallets), established at **Arm time**, not fire
time. This placement is load-bearing: `establish_block_ticker` has its
own 5s connect ceiling, and awaiting that AT FIRE TIME would add up to
5s of synchronous latency to every single fire — directly defeating
the entire point of this codebase's prepare/fire split. `main.rs`'s
`control_loop` establishes it once in the `Arm` handler (same
lifecycle as `warmed_providers`), and `fire_prepared` only ever
receives an already-resolved `Option<BlockTicker>`.

POLL (fallback) fires when the WS path can't be established — sized to
`Config::block_time_ms` (new field, defaults to mainnet's 12000ms,
must be set per-chain), not a fixed interval that only suited mainnet.
Both paths share a `Config::inclusion_timeout_ms` ceiling (default
30000ms); a tx that never gets included reports a `TimedOut` result —
kept structurally distinct in `executor.rs`'s `SendAttemptOutcome` enum
from both a confirmed success/revert AND a genuine broadcast failure,
since "acked but unconfirmed" (may still land) and "never left this
process" (never will) have very different operational meaning and must
never collapse into the same generic error bucket.

**Multi-wallet independence:** each wallet's own broadcast+detection
task runs fully independently (existing per-wallet `tokio::spawn`
structure, unchanged by 14a) — a slow wallet never gates a fast one's
`MintResult`.

**Regression check on revert-detection, done honestly.** No automated
revert-detection test existed anywhere in this repo prior to 14a
(checked directly, not assumed) — it was only ever verified live,
during step 5's original Sepolia dry run. Attempted a fresh live revert
test on the step 14b benchmark token (temporarily lowered its cap below
the already-minted count) — this failed to exercise the actual code
path: `eth_estimateGas` correctly predicted the revert and rejected the
tx before broadcast, so `fire_prepared`'s `receipt.status()` check
never ran in that attempt (confirmed via `audit.log` showing no
`mint_result` event, and on-chain balance unchanged). Forcing a
genuine "estimate succeeds, execution reverts" race needs precise
timing between two competing transactions and wasn't achieved cleanly
in this session. The regression argument that stands instead: the
actual success/revert determination (`if receipt.status() { .. } else
{ .. }`) is byte-for-byte unchanged by 14a's diff — the only thing that
changed is which call supplies `receipt` (`inclusion::wait_for_receipt`
now, `PendingTransactionBuilder::get_receipt()` before), never what's
done with it afterward. All 30 successful fires across 14b's two
benchmarks independently exercise and confirm the success half of that
same unchanged check.

Real request-level tests exist for `inclusion.rs`'s own new logic
(`establish_block_ticker` fails fast on a bad URL, a `watch::channel`
tick genuinely unblocks `changed()` across tasks) — 70 tests total,
`cargo clippy --all-targets -- -D warnings` clean (caught and fixed a
real `large_enum_variant` finding on both new `Included` variants,
boxing `TransactionReceipt` — confirmed live under CI's actual flags,
not the weaker plain `cargo clippy`).

**Expected, not a defect:** live confirmation that the PUSH path itself
engages (rather than always falling back to POLL) needs an environment
without this sandbox's WS/TLS limitation (gap #11) — every live run in
14b confirmed via its own log line
(`inclusion detection: WS subscription unavailable, falling back to
HTTP polling`) that POLL was what actually ran here, same inherited
limitation as `poll_state`/`mempool_watch`/step 13d, not a new gap.

### 14b — real benchmark: methodology, numbers, comparison

**Benchmark tokens, deployed for real, not simulated.** `forge`
couldn't be installed in this sandbox — `foundryup`'s GitHub release
fetch returned 403 (this session's GitHub access is scoped to specific
repos, not general API access; confirmed by testing with the session's
own token, not assumed). Worked around it rather than skipping the
deploy requirement: step 5's original `.testnet-work/seadrop/` checkout
already had `ERC721SeaDrop` fully compiled (`out/ERC721SeaDrop.sol/
ERC721SeaDrop.json`, real bytecode+ABI from that earlier `forge build`)
sitting on disk from the original dry run. Installed `solc` via
`py-solc-x` (pulls from `binaries.soliditylang.org`, not GitHub — a
different, unblocked host) to confirm the toolchain independently, then
deployed fresh, dedicated `ERC721SeaDrop` instances via `web3.py` +
raw signed transactions (same account-management pattern as every
other live test this session), reusing the already-compiled artifact —
one per chain, separate from 13d's dry-run collection and from each
other:
- Robinhood Chain testnet: `0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9`
- Sepolia: `0x6F49dDA46826448cdAf17597B117B66E87c1FC29`

Both configured identically: free (`mintPrice = 0`), `restrictFeeRecipients
= false`, `maxTotalMintableByWallet = 65535` (`uint16`'s max — the
practical ceiling for "unlimited" here; comfortably above any planned
attempt count), live for 7 days from deployment.

**Robinhood Chain testnet's real block time, measured directly, not
assumed from the ~100ms documented figure.** Block timestamps alone
are only 1-second granular, too coarse on their own — averaged over
1000 real consecutive blocks (`span_seconds / 1000`) for a stable
figure: **~227ms**, meaningfully higher than the commonly-cited ~100ms.
Reported as measured, not silently substituted — `block_time_ms = 227`
is what the benchmark config actually used.

**Real results, n=15 per chain, sequential fires from one funded
wallet (same `.testnet-keys/wallet1` throughout), same bot binary,
same methodology:**

| Metric | Robinhood testnet (n=15) | Sepolia (n=15) |
|---|---|---|
| successes | 15/15 | 15/15 |
| send→ack p50 | 139ms | 60ms |
| send→ack p90 | 151ms | 62ms |
| dispatch→inclusion p50 | 978ms | 12,184ms |
| dispatch→inclusion p90 | 1,329ms | 24,247ms |

Sepolia's dispatch→inclusion clusters almost exactly at one block
(~12,180ms) for p50 and two blocks for the p90 stragglers (~24,240ms)
— sanity-consistent with its real ~12s block time and a strong signal
the pipeline is behaving correctly, not an artifact.

**Compared against MintDash's published Robinhood figures (send→ack
p50 117ms, mintDuration p50 136ms) AND against Robinhood's own measured
227ms block-time floor, kept as two separate gaps on purpose — closing
one doesn't imply anything about the other:**

| | vs. MintDash (117/136ms) | vs. measured chain floor (227ms) |
|---|---|---|
| send→ack (139ms) | ~1.2x | n/a (floor doesn't apply to ack) |
| dispatch→inclusion (978ms) | ~7.2x | ~4.3x |

**14a's fix is a real, large, honestly-measured improvement** — 13f's
single n=1 attempt was 7,551ms; the n=15 p50 here is 978ms, a ~7.7x
reduction, entirely from correctly sizing the poll interval to the
real chain, with zero change to broadcast or signing logic.

**Is this now a fair comparison — same detection philosophy — even
though absolute numbers still differ? Answered precisely, not rushed:**
send→ack: **yes**, a fair comparison. Both this bot's number and
MintDash's are measuring the same concept (RPC round-trip time to
accept a broadcast), and the remaining ~22ms gap (139ms vs 117ms) is
plausible and consistent with the RPC-provider-proximity explanation
from @0xSvinci's thread (an uncolocated public testnet RPC from a cloud
sandbox vs. MintDash's own colocated node) — this metric alone is
reasonable evidence supporting a future infra/colocation step's case,
*for this specific number*.

dispatch→inclusion: **no, not yet a fair comparison**, and this is the
answer that actually matters for scoping what comes next. The PUSH
path never engaged in this sandbox (gap #11, expected) — every
measurement here is still fundamentally POLL-based, just correctly
sized now instead of badly oversized. MintDash's `mintDuration` almost
certainly reflects a push/subscription-driven detection philosophy;
this bot's 978ms number is entangled with its own poll-interval floor
(227ms) stacked on top of however many real blocks inclusion actually
takes — and 978ms / 227ms ≈ 4.3 poll cycles suggests real inclusion
itself may be taking several blocks here, not one, which is a
DIFFERENT thing from "our detection is slow." **This gap cannot be
honestly attributed to RPC proximity/colocation** the way the send→ack
gap can, because the detection METHOD itself, not just its network
distance, is still different from MintDash's. Recommendation: an
infra/colocation step's business case is well-supported for the
send→ack number specifically; the dispatch→inclusion number needs the
PUSH path actually validated live (in an environment without this
sandbox's WS/TLS limitation) before it can inform that decision at
all — proposing colocation to fix a gap that might be substantially a
detection-method artifact would be solving the wrong problem.

## Live infra validation + colocation decision (step 15)

Every prior step ran either in this coding sandbox or against testnets
from it — nothing has run anywhere else yet, and gap #11 (WS transport
blocked by the sandbox's own TLS-interception proxy) has shadowed every
report since step 5 with a "should work outside this sandbox" caveat.
Step 15 is split explicitly around what this session can and can't do:
15a/15b are research and preparation, completed now; 15c-15e need a real
VPS the operator provisions — no VPS account or credentials exist in
this session, same reason step 10.5's Cloudflare API token had to come
from the operator directly. **15c-15e have not started as of this
writing** — this section will be updated once the operator confirms the
VPS is live.

### 15a — VPS provider + region recommendation

**UPDATED: AWS EC2, `us-east-1` directly, `t4g.small`.** The original
recommendation below (Hetzner Cloud, Ashburn) is confirmed unavailable
for this operator — not a preference change, a hard constraint. The
swap to EC2 `us-east-1` is not a downgrade from the original reasoning;
it's actually a tighter match to it, since `us-east-1` was always the
literal region the evidence pointed at — Ashburn was only ever a
same-metro approximation of proximity to that region, never the region
itself. The underlying reasoning is unchanged, carried forward rather
than re-derived:
- Alchemy — a named Robinhood Chain infra partner — has its own status
  history naming "US East" as a real serving region for chain traffic
  (a July 2026 Hyperliquid latency incident was explicitly scoped to
  "US East region" in Alchemy's own status reporting), not just a
  generic multi-region claim.
- Robinhood's own production stack visibly depends on AWS `us-east-1` —
  it was among the services affected by the October 2025 `us-east-1`
  outage. Robinhood operates Robinhood Chain's sequencer directly (a
  single Arbitrum-Orbit sequencer, confirmed via Robinhood's own docs);
  the most likely place for it to live is the same region as the rest
  of Robinhood's AWS footprint, not a separate one — inference, not a
  confirmed fact from Robinhood's own docs, stated as such.

**Instance: `t4g.small`** (2 vCPU / 2GB RAM, Graviton/ARM,
~$0.0168/hr) over `t3.small` (identical 2 vCPU/2GB spec, ~$0.0208/hr,
x86_64) — checked against current EC2 pricing directly, not assumed
still accurate, and ~19% cheaper for the same spec. Worth the ARM
architecture specifically because `deploy.sh`'s `source` mode (the
only real deploy path today — no `v*` tag has been cut yet) runs
`cargo build --release` natively on the target machine, which compiles
for whatever CPU it's running on with zero script changes — confirmed
by reading `deploy.sh`, not assumed. Checked the other direction too,
not just assumed compatible: `release.yml` (step 7d) only ever builds
on `ubuntu-latest` with no `aarch64` target, so `deploy.sh release`
mode (a prebuilt tarball) would NOT work on `t4g` until that workflow
gains an ARM target — moot today since `source` is the only option,
but a real, documented limitation for later, not silently glossed
over. `t3.small` is the direct x86_64 fallback if that distinction
isn't wanted. 2GB RAM is less than the original Hetzner CPX11
recommendation's ~4GB class — flagged explicitly rather than silently
substituted; this workload (SQLite, a handful of wallet signers, a few
WS connections) has no obvious reason to need more, but actual memory
use after the first deploy should be watched, not assumed fine. EC2
also bills a `gp3` EBS root volume separately from compute — a real
cost line Hetzner's bundled pricing didn't have (~$1.60/mo for 20GB).

**AWS's networking model needed real, checked additions to the
provisioning checklist, not just a button-label swap** — confirmed by
reading through the actual EC2 launch flow, not assumed identical to
Hetzner's: an explicit region-selector check (AWS remembers your last
region per browser/account, which may not default to `us-east-1` —
launching in the wrong region would silently defeat the entire
recommendation with no error), a Security Group with one inbound SSH
rule restricted to the operator's IP (AWS defaults to deny-all inbound,
unlike Hetzner; no other port is needed — the bot still binds
`127.0.0.1` only, unchanged by any of this), and an EC2 key pair
(`.pem`, downloaded once at launch) in place of Hetzner's SSH-key-upload
flow. **Everything below the OS layer needed zero changes, confirmed
by re-reading each file, not assumed** — `deploy/mint-sniper.service`,
`deploy/deploy.sh`, `deploy/mint-sniper.env.example`, and the `ServeDir`
fix below are all plain-Linux/systemd concerns with no cloud-provider
awareness in them at all; this was a provider swap, not a
re-architecture. Full checklist in `DEPLOY.md`.

<details>
<summary>Original Hetzner/Ashburn recommendation (superseded above, kept for the reasoning trail)</summary>

Two real, independently-sourced signals converged on Ashburn: the same
Alchemy/Robinhood evidence above, plus Ashburn, VA being the same
metro area as AWS `us-east-1` itself — literally the densest
interconnection point on the US East Coast — so a non-AWS VPS provider
with a real Ashburn presence would get the shortest plausible physical
path to both signals without AWS's markup for a workload that doesn't
need it. Hetzner had a real, dedicated Ashburn datacenter with
CPX-series shared-vCPU instances (~2 vCPU/4GB RAM class) sized
correctly for this workload. Alternatives considered at the time:
DigitalOcean's closest East Coast presence is NYC (farther from
Ashburn's interconnection density); Vultr's closest is New Jersey,
similarly farther; AWS `us-east-1` directly was noted even then as the
tightest possible proximity, just judged higher-cost/complexity than
needed — the operator's actual constraint (Hetzner unavailable) is
what settled that tradeoff in AWS's favor, not a re-evaluation of the
complexity judgment itself.

</details>

### 15b — deploy preparation (done now, ready for handoff)

- `deploy/mint-sniper.service` — systemd unit. Dedicated unprivileged
  `mint-sniper` system user (never root), `Restart=on-failure`,
  secrets loaded from a 0600 `EnvironmentFile` (never inline in the
  unit — unit files under `/etc/systemd/system` are typically
  world-readable, same "secrets never touch a loosely-permissioned
  file" standard `config.toml`/`.sniper-token`/`identity.db` already
  get).
- `deploy/mint-sniper.env.example` — template for that env file,
  matching `config.toml`'s existing `private_key_env`-name-not-value
  convention exactly.
- `deploy/deploy.sh` — idempotent; supports building from source
  (`git clone`/`pull` + `cargo build --release` + `npm run build`, the
  only real option as of this writing) or from a GitHub release
  tarball (`deploy.sh release` — works once step 7d's workflow has
  actually produced one; no `v*` tag has been pushed yet, checked
  directly against the repo's tag list, not assumed). Never touches
  `config.toml`, the env file, or the identity DB — a redeploy only
  ever rebuilds code.
- **Found and fixed a real gap while preparing this, not a
  hypothetical one:** `ui/README.md`'s "Prod" section had described
  `tower_http::services::ServeDir` serving `ui/dist` as the plan since
  before step 10, but it was never actually wired into `api.rs`'s
  router — confirmed by reading the code, not the docs. A deploy
  script written against the documented plan would have shipped a
  binary with no way to serve the UI at all on a real VPS. Fixed in
  `api.rs`'s `router()`: `ServeDir` with an `index.html` fallback (this
  app has no client-side router — confirmed, not assumed — so the
  fallback exists purely for PWA reload-to-a-cached-path safety),
  mounted outside `require_token_or_session` on purpose — the static
  shell has to load before the browser can even call `GET /api/token`,
  so auth applies to the API calls the shell makes, never to the shell
  itself. `README.md`'s stale "the API has no auth" security note
  (accurate before step 7b, false since) and its Tailscale/SSH-only
  reachability advice (predating step 10.5's Cloudflare Tunnel option)
  were also found stale during this same cross-check and corrected.
- Release tarball note: `release.yml` names the UI directory `ui-dist`
  inside the archive; the binary expects `ui/dist` relative to its
  CWD. Reconciled in `deploy.sh`'s `release` mode rather than editing
  `release.yml` itself, since changing that workflow would need
  re-verification by actually cutting a tag — out of scope for this
  step.

### 15c-15e — gated behind real VPS provisioning

Not started. Once the operator confirms a VPS is live and the bot is
deployed on it (`DEPLOY.md`), this section will be replaced with: real
confirmation that `subscribe_blocks`/`subscribe_full_pending_transactions`/
14a's PUSH path connect cleanly outside this sandbox's TLS limitation
(closing gap #11 for real, with actual evidence — a connected
subscription, a real detected trigger — not "should work now"); a
re-run of 14b's benchmark methodology with PUSH actually engaged; and a
final, per-metric colocation recommendation — send→ack's ~22ms gap
already has a plausible proximity explanation from 14b that this step
should either confirm or correct with real numbers from an
actually-well-placed host; dispatch→inclusion's verdict depends
entirely on numbers that don't exist yet, per 14b's own explicit
gating (don't propose self-hosting a node for a gap that might still be
a detection-method artifact until PUSH is validated live).

## Live first deploy — real findings (step 16)

The operator's first real attempt to actually deploy onto the `t4g.small`
EC2 instance step 15 recommended hit three real, live-confirmed
problems, in this order — not hypothetical, not "might happen." Folded
directly into `DEPLOY.md`'s checklist (not just noted here
retrospectively) so a future deploy doesn't repeat them; documented
here too as the reasoning trail.

**1. `mint-sniper` couldn't see the Rust toolchain at all.** Installing
Rust as the `ubuntu` login user (the natural result of running
rustup's one-liner as yourself, which is what `DEPLOY.md`'s original
section 2 said to do) puts the toolchain in `~ubuntu/.cargo` —
`ubuntu`'s home directory has default `750` permissions
(`drwxr-x---`), which blocks every other user, including
`mint-sniper` (the account `deploy.sh` actually builds as), from even
traversing into it. Surfaces as `cargo: command not found` under
`sudo -u mint-sniper` — reads exactly like a failed install, not a
permissions problem, which is why this is worth stating explicitly
rather than trusting a future operator to diagnose it from the error
message alone. Fixed in `DEPLOY.md` section 2: create the
`mint-sniper` system user FIRST (before Rust, not after — deploy.sh's
own idempotent `useradd` check makes doing this early harmless), then
install Rust FOR that user (`sudo -u mint-sniper -H bash -c
'curl ... | sh'`), landing the toolchain in `/opt/mint-sniper/.cargo`
where `deploy.sh` actually looks for it. Also hardened `deploy.sh`
itself: its cargo-found check now runs as `$SERVICE_USER` (not root —
checking root's own PATH would have passed even with this exact bug
present), and the build invocation explicitly sources
`$HOME/.cargo/env` rather than relying on a non-interactive `bash -c`
to have sourced `.bashrc` the way an interactive shell would.

**2. The 20GB storage recommendation was already documented and still
got missed live.** `DEPLOY.md`'s original section 1 step 5 already
said to bump the root volume to 20GB — the recommendation existing
wasn't enough to prevent the wizard defaulting back to 8GB and nobody
catching it before launch. Root filled to 100% partway through
re-copying the Rust toolchain while fixing problem #1. Fixed by
turning the recommendation into a mandatory verification step
(`df -h /`, confirm ~20GB, immediately after first SSH access, before
anything else) with a documented live-recovery path (EBS Modify volume
→ `growpart` → `resize2fs`, no instance restart needed) for exactly
this miss happening anyway — a recommendation alone was proven
live not to be sufficient.

**3. `cargo build --release` was OOM-killed even after both fixes
above.** This codebase's release profile deliberately uses LTO +
`codegen-units=1` (a real, intentional tradeoff — not something to
change to dodge this), and LTO's link step spikes memory hard with no
headroom on `t4g.small`'s 2GB RAM — `signal: 9, SIGKILL` from the
kernel OOM killer. A 4GB swapfile fixed it immediately (next attempt
compiled clean in under 2 minutes). This supersedes step 15a's earlier
"2GB is less than the ~4GB original recommendation, watch actual
memory usage" framing — that undersold the real risk. It isn't a soft
thing to monitor after the fact; without swap, the release build does
not complete at all on this instance size. `DEPLOY.md` section 2 now
makes a 4GB swapfile (with the `/etc/fstab` persistence line, so it
survives a reboot) a mandatory, non-optional first-time-setup step on
`t4g.small` specifically, not an optional performance tweak.

## The bot's own WS connection failed with "Must be authenticated!" (step 17)

**Symptom, live-confirmed on the real deployed VPS:** the bot crash-looped
at boot with `server returned an error response: error code -32600: Must
be authenticated!` from Alchemy, on the exact same `wss://` URL and key
that both a plain HTTP curl AND a bare Node.js `ws` client (sending
`eth_chainId`) succeeded against. This ruled out the credential/Alchemy
app itself — something specific to how *this codebase* opens the
connection differed from a bare WS client in a way Alchemy's endpoint
read as an auth attempt gone wrong.

**Root cause, confirmed by reading alloy 2.4.1's actual source (not
assumed from memory), both a code fact and a config/docs gap:**
`alloy-transport-ws`'s `WsConnect::new(url)` parses the given URL and, if
it contains userinfo (`wss://user:pass@host/...` or `wss://user@host/...`),
auto-extracts it via `Authorization::extract_from_url` and injects an HTTP
`Authorization` header into the WebSocket upgrade handshake
(`alloy-transport-ws-2.4.1/src/native.rs`'s `IntoClientRequest` impl,
backed by `alloy-transport-2.4.1/src/common.rs`'s
`Authorization::extract_from_url`). A bare `ws` client never does this —
it sends whatever URL you give it as a plain WS upgrade with no
credential-flavored header at all. Alchemy's auth model for this codebase
is a path-embedded API key (`/v2/<KEY>`), never URL userinfo — so this
auto-extraction is never wanted behavior here, and a `ws_rpc_url` with any
stray `@`/`:` before the host (this exact `config.toml` had already
produced two other copy-paste corruptions earlier that same deploy night
— a missing closing quote, a truncated key) would silently make alloy
send a bogus Basic-auth header Alchemy was never expecting, while a bare
client sending the identical-looking URL correctly sends none. The three
alternative 17a hypotheses (an alloy-internal auto `eth_chainId`/subscribe
call at connect time despite `disable_recommended_fillers()`; some other
non-standard header/handshake step) were checked against alloy's actual
`ClientBuilder::ws`/`connect_ws`/`PubSubConnect::connect` source and ruled
out — connection setup issues nothing beyond a standard WS upgrade (plus
the conditional Authorization header above) before normal JSON-RPC
traffic begins.

**This session could not confirm the operator's real `config.toml` was
actually corrupted this way** — no access to that file exists from this
sandbox, and the fix stands regardless of whether that was the literal
trigger this specific night. What shipped:

1. **`Config::validate()`** (`src/config.rs`) now rejects any
   `ws_rpc_url`/`http_rpc_urls` entry containing embedded userinfo
   credentials outright, at config load/save time — not silently letting
   it through to surface as an opaque connect-time auth failure hours
   later. Same "catch a bad shape at startup/save time" principle as
   every other check in that function.
2. **Error context, all four `connect_ws` call sites**
   (`copymint.rs::watch_once`, `inclusion.rs::establish_block_ticker`,
   `watcher.rs::run_state_poll_watcher`, `watcher.rs::run_mempool_watcher`)
   now wrap the connect failure with which code path was connecting AND a
   *redacted* form of the RPC URL (scheme+host only, via the new
   `config::redact_rpc_url` — never the raw URL, since the path segment
   IS the API key and this project's secrets-never-touch-a-log rule
   applies here same as everywhere else). `establish_block_ticker`
   specifically used to swallow the connect error entirely (by design —
   HTTP-polling fallback is the correct response to WS being unavailable)
   but gave the operator zero information about *why* it failed; it now
   logs the real reason via `warn!` before falling back, same "surface
   the real reason, don't swallow it" standard as step 3b's revert-reason
   fix. A future occurrence of this — or any other WS connect failure —
   is now diagnosable from the systemd journal alone: which watcher, which
   host, and the real underlying error text, no live Node.js test harness
   needed to even start isolating it.
3. **`config.example.toml`** now has an explicit warning above
   `ws_rpc_url` explaining the path-vs-userinfo distinction and why it
   matters, so a future operator copying this file has the context before
   hitting the failure, not after.

**Follow-up, same night: the fix deployed correctly but the operator saw
the exact same unwrapped error anyway.** This is the actual answer to
"which boot-path call site was missed" — none was. An exhaustive re-grep
(`grep -rn "WsConnect::new\|connect_ws\|extract_from_url" src/`, and
separately confirmed no bare `tokio_tungstenite`/raw WS client exists
anywhere outside these four — `api.rs`'s and `state.rs`'s `WebSocket`
hits are axum's own *inbound* `/ws/events` upgrade for the browser UI,
unrelated to any outbound Alchemy connection) turned up the same four call
sites and nothing else. `main.rs`'s own synchronous startup path (before
any watcher spawns) makes zero WS calls at all — only `http_rpc_urls`
ones (`seadrop::fetch_public_drop`, `wallet::load_wallets`,
`connect_http`) — and `alloy-rpc-client-2.4.1/src/builtin.rs` (checked
directly) confirms the `Http(url)` transport variant never calls
`Authorization::extract_from_url` at all — only the `Ws` variant does. So
17a's finding is WS-only, confirmed, not something to also guard against
on the HTTP side.

**The real gap: `bus::log` (`bus.rs::log`) only pushes onto the internal
event bus the browser UI's `/ws/events` stream consumes — it never calls
a `tracing` macro, so it never reaches stdout, and therefore never reaches
`journalctl`.** Two of the three failure paths that route through it were
logging ONLY that way:
- `copymint.rs::run_copymint_watcher`'s own error handler for
  `watch_once`'s failures.
- `main.rs::spawn_supervised_watcher` — the wrapper both
  `run_state_poll_watcher` and `run_mempool_watcher` failures flow
  through, which logs and disarms on error.

17b's redacted-URL context was correctly attached to the underlying
`anyhow::Error` in both cases — it was genuinely present in the
`{e:#}` formatted into the `bus::log` call — but since that call never
reaches the journal, `journalctl -u mint-sniper` would show *nothing at
all* from these two paths, old error text or new, watcher.rs's third path
(`inclusion.rs::establish_block_ticker`'s `warn!`) was never affected —
it already called `tracing::warn!` directly, not `bus::log` — which is
why it's the one to imitate, not the exception. Fixed by adding a
`tracing::error!` call alongside the existing `bus::log` call in both of
the other two spots, so the exact same message reaches both the UI and
the journal. **Neither a code bug in the WS-connect logic itself (17a/17b
already had that right) nor a config gap — an observability gap in this
codebase's own two error-reporting call sites**, invisible until an
operator actually relied on `journalctl` alone per 17c's own instructions
and found nothing there.

Also added `main.rs::tests::every_ws_connect_call_site_is_accounted_for`
— a regression guard, not just a one-time check: it greps `src/` at test
time for `= WsConnect::new(` call sites and fails if the count or the set
of files changes, so a fifth call site added later can't silently ship
without the same redacted-URL-context + tracing + bus::log treatment.

**Operator verification (this session cannot reach the live VPS to
confirm directly, same boundary as every other live-deploy step):**
```
cd /opt/mint-sniper/repo && sudo -u mint-sniper git pull --ff-only origin main
sudo -u mint-sniper -H bash -c "source \$HOME/.cargo/env && cd /opt/mint-sniper/repo/mint-sniper && cargo build --release"
sudo systemctl restart mint-sniper
sudo systemctl status mint-sniper --no-pager   # expect: active (running), not a crash loop
sudo journalctl -u mint-sniper -n 50 --no-pager  # NOW actually shows watcher/copymint errors — before this
                                                  # follow-up, bus::log-only paths were invisible here regardless
                                                  # of 17b's fix. Look for: the config load failing outright with
                                                  # "RPC url contains embedded credentials" (confirms 17a's
                                                  # hypothesis was the trigger — fix ws_rpc_url and restart), OR a
                                                  # "watcher exited with an error: ... opening the WebSocket RPC
                                                  # connection to wss://<host>/*** ..." / "copymint watcher error:
                                                  # ..." line with the real underlying reason, OR nothing at all
                                                  # (service came up clean).
```
If `journalctl` now shows the new validate() rejection instead of a crash
loop, that confirms 17a's hypothesis was the actual trigger — fix
`config.toml`'s `ws_rpc_url` to remove anything before the host and
restart again. If it instead shows a "watcher exited with an error"/
"copymint watcher error" line (now visible for the first time — this
follow-up's own fix), that reason, not 17a's userinfo theory, is the real
cause; treat that text as authoritative over anything guessed here. If
the service comes up clean with no error at all, the fix already resolved
it silently (nothing further needed). Either way,
confirm live RPC connectivity itself via the bot's own `/api/status`
endpoint or a log line showing a successful block subscription — not just
"the process didn't crash."

**16b — `deploy/setup-cloudflared.sh`, prepared, not run.** Same
division of labor as `deploy.sh` and the EC2 provisioning itself: this
session has no Cloudflare account access, so the script is
ready-to-hand-off, never executed here. Three explicit modes
(`install` / `login-and-create` / `service`) matching which steps are
scriptable (installing the apt package, writing the systemd service)
versus which genuinely need the operator's own interactive browser
auth (`cloudflared tunnel login`/`tunnel create` — no flag exists to
skip this, and the script says so rather than guessing at one). Points
at `127.0.0.1:4117` — confirmed directly against `main.rs`'s
`API_BIND_ADDR` before writing the script, not assumed unchanged since
step 7b. Installs cloudflared as a persistent systemd service (`cloudflared
service install`) rather than `ui/README.md`'s original foreground
`cloudflared tunnel run` walkthrough, which doesn't survive a reboot or
a dropped SSH session — that walkthrough now points at this script for
the actual VPS deploy. Does NOT create the Cloudflare Access
application/policy or touch the Cloudflare API token — both stay
manual dashboard steps under the operator's own account, per step
10.5c's existing design, unchanged by this step. The script's own
header comment points back to step 10.5a's WebAuthn-origin decision
rather than restating it — whatever hostname the operator routes here
becomes the canonical origin, and any passkeys registered under a
different origin stop validating the moment that switch happens
(expected, not a bug).

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
- **After every push, confirm the real GitHub Actions run went green
  before reporting any step done — local build/test/clippy passing is
  necessary but has been proven NOT sufficient on its own.** Incident:
  a step 9g commit broke `.github/workflows/ci.yml`'s YAML syntax (an
  edit deleted a `with:` block's only real key while expanding the
  comment above it, leaving `with:` mapped to nothing — invalid per
  GitHub's schema). That failure happens at workflow-parse time, before
  any job ever dispatches, so every run from 9g through 10h (13
  consecutive runs, CI 13-25) showed 0 jobs total — yet every one of
  those sub-steps was reported "done, clippy/build/test clean, pushed"
  without anyone actually opening the run and checking. On top of that,
  local verification throughout 10b-10h ran plain `cargo clippy
  --all-targets`, not CI's actual `cargo clippy --all-targets -- -D
  warnings` — a real dead_code warning (session.rs's `Session` struct)
  that had been logged as an acceptable pending warning was in fact a
  hard compile error under CI's real flags, and would have failed the
  rust job independently of the YAML bug. **The fix, going forward:**
  after any push, use the GitHub MCP Actions tools (or the equivalent
  API) to fetch the resulting run and its per-job status, wait for
  `status: completed`, and read every job's `conclusion` individually —
  not just the top-level run conclusion, and not "no news is good
  news." Don't reuse local verification commands that differ from CI's
  actual invocation (flags, working directory, etc.) as a stand-in for
  checking CI itself. A step is not done until this has actually been
  observed once, this session, against a real run.
