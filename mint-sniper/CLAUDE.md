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

**SUPERSEDED by step 15f — kept below for the reasoning trail, not as
the current numbers.** Every figure and every "cannot yet attribute
this to X" hedge below was written before the PUSH path had ever run
outside this coding sandbox — the whole point of gap #11. Step 15f has
the real thing: a genuine n=15 PUSH-based run (zero POLL fallback) plus
direct on-chain evidence resolving exactly the question this section
could only speculate about. Read 15f for the actual current numbers
and verdict; read below for how the investigation got there.

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
from the operator directly. **The VPS is now live (step 16/17). 15c-15e
are prepared as ready-to-run scripts (`deploy/benchmark-token.sh`,
`deploy/run-benchmark.sh`) and `DEPLOY.md` §9 instructions — not yet
executed, since this session still can't reach that VPS directly.** See
the "15c-15e" subsection below for what's ready and what genuinely
still needs the operator to run it.

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

### 15c-15e — prepared and ready to run; not yet executed

The operator's VPS is now confirmed live (step 16/17's real first
deploy) — the environment this whole gap #11 closure has been waiting
for since step 5 finally exists. This session still can't reach it
directly (same boundary as every prior live-deploy step), so 15c-15e
are prepared as ready-to-run scripts + `DEPLOY.md` instructions
(§9), not executed here:

- **`deploy/benchmark-token.sh`** — `check <addr>` calls
  `getPublicDrop` directly and compares `endTime` to now to confirm
  step 14b's original benchmark token
  (`0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9`) is still live;
  `redeploy` deploys a fresh, identically-configured one via Foundry
  (`forge create` + `cast send updatePublicDrop`) if it's expired —
  likely, given it was only ever deployed "live for 7 days." Notes
  explicitly that this session never got Foundry installed in its own
  sandbox (`foundryup`'s GitHub fetch 403'd against this session's
  scoped network access) but that this was a sandbox-specific block,
  not a Foundry one — a real VPS with normal internet access should
  install it the standard documented way.
- **A real gap found and fixed while preparing 15d, not hypothetical:**
  the exact log line 14a's own doc comment names as proof PUSH engaged
  (`inclusion detection: WS push path established` /
  `... unavailable, using HTTP poll fallback`) was only ever reachable
  by watching the live browser UI at the precise moment of Arm —
  `bus::log` never reaches `journalctl` (step 17's finding) AND
  `audit.rs`'s writer explicitly skips `ServerEvent::Log` too (its own
  comment: "not what RUNBOOK.md's checklists need a durable record
  of"). That made 15d's own ask — "check the event feed/audit log for
  whichever log line distinguishes push vs. poll mode" — genuinely
  impossible to satisfy after the fact on a headless VPS with the
  tooling as it stood. Fixed by adding a `tracing::info!` call
  alongside the existing `bus::log` call for this specific line (same
  pattern as step 17's fix), so `journalctl -u mint-sniper` now shows
  it durably. `DEPLOY.md` §9's 15d instructions are built around this
  fix, not the old UI-only visibility.
- **`deploy/run-benchmark.sh`** — automates the full n=15+ loop
  (`/api/arm` → wait for the resulting `mint_result` audit.log entry →
  repeat), replacing the manual per-fire commands step 17's live
  debugging session had to resort to. Prints p50/p90 for both
  `send_to_ack_ms` and `dispatch_to_inclusion_ms` (same two metrics
  14b measured), plus a push-vs-poll count cross-checking 15d across
  the whole run. Relies on `mint_mode = "seadrop"` forcing
  `trigger_mode = "timestamp"` with the drop's real, already-past
  `startTime` at boot (confirmed directly against `main.rs` — not
  assumed) so each `/api/arm` alone triggers a fire within about a
  second, no separate manual trigger call needed. Explicitly flags,
  rather than silently working around, the two things that need the
  operator's own judgment: confirming a testnet-ETH faucet transfer
  actually landed before spending 15+ real mints against that balance,
  and that a step-up-TOTP-gated instance (identity/step 10c enabled)
  can't have this loop automated at all — a human would need to supply
  a fresh code before every single arm, which the script does not
  attempt to fake.

**Follow-up, first real live run of `benchmark-token.sh check`: a real
parsing bug, found on the VPS, not hypothetical.** `cast call`'s
DEFAULT text output annotates any integer it judges "large" with a
human-readable bracket — a real captured line looked like
`1787557476 [1.787e9]`, not the bare `1787557476` the script originally
assumed. Feeding that whole string into bash's `(( ))` arithmetic threw
a syntax error and crashed the `check` subcommand outright. The
underlying RPC call and value were confirmed correct both before and
after the fix (the operator manually verified the step 14b benchmark
address (same one line 1106/1346 above already reference) was still
live with ~2.8 days remaining) — this was purely a text-parsing bug,
never a wrong call or a wrong contract. Fixed by extracting just the leading digit
run with `grep -oE '[0-9]+' | head -1` instead of `tr -d '[:space:]'`
alone, deliberately NOT switching to `cast call --json` + `jq`: `jq` is
not installed by default on a stock Ubuntu VPS either (confirmed the
same night — nothing had installed it), so reaching for it here would
trade one undocumented tool assumption for another; `grep`/`sed` ship
with bash everywhere. Audited `run-benchmark.sh` for the identical
pattern and found none — it never calls `cast` at all; its only
external numeric parsing is `jq -r` over `audit.log`, which is plain
`serde_json` output from this codebase's own `audit.rs`, never cast's
human-readable annotation. Its `jq`/`curl`/`python3` prerequisite check
now names the `apt-get install` fix directly instead of just stating
the tool is missing, same tooling-assumption lesson applied where it's
actually relevant there.

**Also found while responding to this: no shell-script test harness
existed anywhere in this repo** — confirmed by reading
`.github/workflows/ci.yml` directly, not assumed; its three jobs
(Rust, UI, Secret scan) never touch `deploy/*.sh` at all. Added
`deploy/tests/test-benchmark-token.sh` — plain bash, no new framework
dependency, stubs `cast` with the exact bracket-annotated output shape
this bug produced and asserts the `check` subcommand handles a
still-live drop, an expired drop, and a no-drop-configured contract
without crashing. Verified this test actually catches the original bug
(not just the fix): temporarily reverted the parsing line back to the
buggy `tr -d '[:space:]'` version, confirmed the test suite failed with
the exact same arithmetic syntax error the operator saw live, then
restored the fix and confirmed it passes clean. Wired into CI as a new
`deploy-scripts` job (`.github/workflows/ci.yml`) so this specific
regression, and this whole class of "external command output fed
straight into bash arithmetic" bug in this file, can't reach a live VPS
silently again.

**Follow-up: `run-benchmark.sh` fired against mainnet, not testnet — a
real, missing-feature bug, found live.** The version of the script
described above was written as if config.toml would already be pointed
at Robinhood Chain testnet before it ran — but nothing in the script,
or in `DEPLOY.md`'s instructions, actually made that true. Confirmed
live: the bot doesn't hot-reload `ws_rpc_url`/`http_rpc_urls` (read
once at `Config::load` time, per `main.rs` — a `systemctl restart` is
required to pick up a config change, not just editing the file), so the
script fired straight against whatever network config.toml already had
on the real box (mainnet). The pre-flight check correctly reported
`0.000000000000000000` balance on all three configured wallets (real
Robinhood testnet-funded wallets, on a chain the bot wasn't actually
connected to) — and the script proceeded to fire anyway. The balance
gate this section already described as a prerequisite ("this script
does not check this for you") had also never actually been built as a
hard stop; it only printed a warning about wallet count, never balance.

**Fixed — `run-benchmark.sh` now owns the whole network swap, not just
the firing:**
1. Backs up `config.toml` to `config.toml.backup` — but first checks
   whether a backup already exists and REFUSES to proceed if so. A
   naive unconditional backup here would be a real hazard: if a prior
   run crashed between backup and restore, `config.toml.backup` right
   now holds the operator's real original config — blindly copying the
   (possibly still testnet-swapped) `config.toml` over it would
   permanently destroy the only copy of the real one.
2. Rewrites `config.toml`'s network/target fields
   (`ws_rpc_url`/`http_rpc_urls`/`mint_mode`/`nft_contract`/
   `fee_recipient`/`quantity_per_wallet`/`block_time_ms`) to point at
   testnet and the confirmed-live benchmark contract from step 15c —
   pulled from `benchmark-token.sh check`'s own machine-parseable
   `BENCHMARK_NFT_CONTRACT=` output line, never a second, independently
   hardcoded copy of that address. Extracted into its own file,
   `deploy/lib/swap_config_to_testnet.py`, specifically so this logic
   is unit-testable on its own (see below) without needing systemd,
   sudo, or a real bot at all.
3. Restarts the systemd service and polls `GET /api/status` for a real
   200 response — not a fixed `sleep` — before proceeding.
4. THEN runs the actual n=15+ fires, exactly as before.
5. Restores `config.toml` from the backup.
6. Restarts the service again.
7. Verifies the restore byte-for-byte (`cmp -s`) before EVER reporting
   success — this is the actual safety guarantee the whole redesign
   exists for. Deliberately stricter than the literal spec in one way:
   even a byte-for-byte-clean restore is not reported as verified
   unless the service also comes back to a real healthy `/api/status`
   afterward — a file matching on disk while the process is crash-
   looping on it isn't actually "back to normal."

Steps 2-6 are wrapped in a single `trap ... EXIT` (`cleanup()` in the
script), so an interrupted run — an operator Ctrl+C, same as happened
live the night this was found, or the script erroring out mid-fire —
still runs the restore-and-verify before the process actually exits.
The trap's own critical command does not lean on `set -e` propagating
correctly inside trap handlers (a known cross-version inconsistency in
bash) — its exit status is checked explicitly instead.

**The pre-flight balance gate, actually built this time.** Extracted
into `deploy/lib/check_wallet_balances.py` (same "make it independently
testable" reasoning as the swap script) — reads a `GET /api/status`
JSON body, flags every wallet below `MIN_BALANCE_ETH` (default 0.01,
overridable), and `run-benchmark.sh` now hard-stops before firing if
any are — printing exactly which wallets need funding, the literal
scenario this section describes as the live incident. Given
`balance_poll_loop` only refreshes every 15s (`main.rs`), the script
waits 20s after the post-swap restart before trusting `balance_eth` at
all, so it doesn't read stale pre-swap data as if it were current.

**Verification — what could genuinely be tested here, and what
couldn't.** This session still can't reach the live VPS, so the
root-required, systemd-restarting orchestration in `run-benchmark.sh`
itself has not run end to end anywhere but that VPS — `DEPLOY.md` §9
has the precise operator-run verification steps for that part,
including an explicit "confirm `config.toml.backup` no longer exists
after a successful run" check. What COULD be tested, and was:
- `deploy/tests/test-swap-config-to-testnet.py` — the config-rewrite
  logic against a scratch config file shaped like the real
  `config.example.toml`, including a multi-line `http_rpc_urls` array
  and a commented-out example block reusing the same key names
  (specifically checking the regex-based rewrite doesn't touch a
  commented line just because it shares a key name with a real one —
  the exact kind of naive-regex trap the cast-bracket bug earlier this
  step was a version of). 13 assertions, all passing.
- `deploy/tests/test-check-wallet-balances.py` — the balance gate
  against mocked `/api/status` JSON, including the exact live incident
  shape (three wallets at 0.000000000000000000), a boundary case
  (balance exactly equal to the threshold passes, matching the script's
  strict less-than), and an empty wallet list. 13 assertions, all
  passing.
- `deploy/tests/test-config-backup-restore.sh` — the two bash idioms
  the safety net depends on (`[[ -f "$BACKUP_PATH" ]]` refusing a
  second run, and `cmp -s` correctly distinguishing a clean restore
  from a corrupted/partial one) against scratch files. 4 assertions,
  all passing.

All three are wired into CI's `deploy-scripts` job alongside the
existing `test-benchmark-token.sh`, so this whole class of bug — and
this specific one, if it ever regresses — fails a real, per-push CI
check instead of reaching a live VPS silently again.

**Follow-up: fire 1/15 of an actual n=15 run crashed on result
parsing — with the mint pipeline itself confirmed genuinely correct.**
Real signal, not a regression: `journalctl` showed all 3 configured
wallets firing, all 3 confirming via the real WS PUSH path
(`method="push"` on every result), with real `send_to_ack_ms`/
`dispatch_to_inclusion_ms` values logged for each — the swap, restart,
balance gate, and actual mint pipeline from the fix above all worked.
The script crashed reading its OWN result back out of `audit.log`:
```
jq: error (at <stdin>:1): Cannot index string with string "success"
```
**Root cause, confirmed directly from `audit.rs`'s source, not
guessed:** `AuditRecord` serializes its `detail` field with
`#[serde(flatten)]`:
```rust
struct AuditRecord<'a> {
    ts: u64,
    event: &'a str,
    #[serde(flatten)]
    detail: serde_json::Value,
}
```
`flatten` means the `MintResult` object's own keys —
`success`/`send_to_ack_ms`/`dispatch_to_inclusion_ms`, AND a
*separate*, differently-shaped `detail` field that's actually a plain
STRING (a human-readable message like `"confirmed"`) — all land
directly on the TOP-LEVEL audit record. There is no nested
`"detail": {...}` wrapper the way the Rust field name suggests. A real
line looks like:
```json
{"ts":1755800001,"event":"mint_result","address":"0x...","success":true,"detail":"confirmed","trigger_to_dispatch_ms":4,"send_to_ack_ms":139,"dispatch_to_inclusion_ms":978,"prepare_age_ms":30}
```
`run-benchmark.sh`'s `.detail.success` was therefore indexing that
top-level `detail` STRING with `"success"` — exactly the observed
error, reproduced locally by feeding a record reconstructed straight
from this struct through the identical `jq -r '.detail.success'`
expression, byte for byte.

**Fixed:** the field-extraction logic was pulled out of
`run-benchmark.sh` into `deploy/lib/find_mint_result.sh` — same
"independently testable, no drift between what's tested and what
actually runs" reasoning as the swap/balance-gate scripts before it —
and now reads the flattened top-level fields directly, projecting the
matched record down to `{success, send_to_ack_ms,
dispatch_to_inclusion_ms}` so the rest of the pipeline (the results
file, the Python p50/p90 summary) never has to know about the
`detail`-naming collision at all. The embedded Python summary parser
had the exact same bug (`json.loads(line)["detail"]`) — fixed the same
way, reading the record directly.

**Verified two ways, not just fixed and trusted:** (1) reproduced the
exact crash locally by running `.detail.success` against a record
reconstructed directly from `audit.rs`'s own serialization code — not
a paraphrase of the live report; (2) added
`deploy/tests/test-find-mint-result.sh` (9 assertions: a real-shaped
success record, a reverted/failed record, a `TimedOut` record with
`dispatch_to_inclusion_ms: null`, no-match-yet, and multiple wallets
firing per arm) and confirmed it actually catches the bug by
temporarily reintroducing the old `.detail.X` nesting assumption into
`find_mint_result.sh` — the test suite crashed with the identical `jq`
error the operator saw live — then restored the fix and confirmed
clean. Wired into CI's `deploy-scripts` job alongside the others.

**Root cause classification:** purely a script-side result-parsing
bug — a mismatch between what `#[serde(flatten)]` actually produces
and what the shell script assumed, not a bug in the mint pipeline, the
config swap, the balance gate, or anything upstream of reading the
result back. All of that is now independently confirmed working
correctly by the very journalctl evidence that surfaced this bug.

**A genuinely new, unresolved finding surfaced by that same single
successful fire — flagged, investigated as far as this session can,
NOT explained away.** Tonight's one confirmed-good PUSH-path fire (3
wallets, `method="push"` on every result — not a polling-interval
confound, the first time that's ever been true in this project's
history) measured `dispatch_to_inclusion_ms` of **2722 / 2876 / 3023**
— roughly **12x** step 14b's real, measured Robinhood Chain testnet
block time (~227ms, from 1,000 consecutive block timestamps). Sepolia's
14b numbers validated the pipeline by clustering almost exactly at one
block; these numbers are nowhere near that. This can no longer be
dismissed as a detection-method artifact the way every prior benchmark
had to be — it's a real PUSH-path number, and it's the first one that
ever existed.

**Public documentation research (WebSearch, `docs.robinhood.com/chain`,
current as of this writing) directly informs, and substantially
reframes, this investigation:**
- Robinhood Chain's sequencer uses **first-come-first-served ordering
  — "higher gas fees do not confer priority."** This makes the
  gas-pricing hypothesis (a too-low `priority_fee_multiplier`/
  `max_priority_fee_gwei_cap` for testnet conditions) **unlikely to be
  the actual cause**, unlike on a standard EVM chain where it would be
  the obvious first suspect. Worth checking anyway (see tooling below),
  but the documented model itself argues against it.
- There is a **separate, low-latency sequencer feed**
  (`wss://feed.testnet.chain.robinhood.com`) distinct from what a
  standard RPC provider (Alchemy, in this bot's case) exposes,
  explicitly documented for full nodes wanting the fastest possible
  view of new blocks. This makes a **different** hypothesis newly
  plausible: real on-chain inclusion may have been fast, while THIS
  bot's subscribed RPC node was itself slow to learn about / propagate
  that new block to its `subscribe_blocks()` subscriber — meaning
  `dispatch_to_inclusion_ms` would be measuring RPC-node lag, not
  genuine sequencer delay. This is a real, RPC-quality-dependent
  explanation, but a fundamentally different one from send→ack's "how
  fast did we hear a broadcast was accepted" latency — it does NOT
  automatically inherit send→ack's "proximity explains it" conclusion,
  and must not be bucketed with it without actual evidence either way.

**A real, concrete gap found while trying to investigate this at all:
the bot never logged the actual gas price used, anywhere.**
`executor.rs::prepare_fire` computes `base_fee`/`wallet_priority_fee`/
`max_fee_per_gas` but never logged any of them — not to `tracing`, not
to `audit.log` — so there was no way to check item 1 above (real gas
conditions vs. what was configured) after the fact at all. Fixed: the
existing `"wallet prepared"` `info!` log line (already `tracing`-based,
durable in `journalctl`) now carries `base_fee_wei`,
`wallet_priority_fee_wei`, and `max_fee_per_gas_wei`.

**New tooling prepared, not run — this session still cannot reach the
real chain or a real fire's tx hash:**
`deploy/lib/diagnose_inclusion_delay.py` takes a confirmed tx hash and
its logged `dispatch_to_inclusion_ms`, fetches the real receipt +
surrounding block timestamps, and computes how many REAL blocks
actually separated dispatch from inclusion — the one measurement that
can actually distinguish "real sequencer delay" from "our RPC node's
own propagation lag," which no amount of reasoning from this sandbox
can substitute for. ≤2 blocks elapsed → node/subscription lag, not real
delay; more than that → genuine on-chain delay, and per the FCFS
finding above, NOT expected to respond to raising the gas-price config
knobs. `run-benchmark.sh`'s own summary output now points at this tool
automatically for any outlier fire. Tested
(`deploy/tests/test-diagnose-inclusion-delay.py`, 9 assertions) against
fixture blocks modeling both this investigation's actual numbers (a
~2722ms duration against a 227ms block time, correctly classified as
real delay) and the fast-inclusion/slow-detection counter-scenario —
all against an injectable RPC dependency, no real network needed for
the test itself.

**What this session can honestly report, and what it cannot:** the
public-docs research rules gas pricing IN as checkable but OUT as the
likely primary cause, and identifies RPC-node propagation lag as a
newly plausible, well-motivated alternative — both real findings. But
**no full n=15 run exists yet**, and this single sample (n=1, all three
values from the SAME arm, not independent draws across arms) cannot be
treated as a distribution — the task's own item 3 is exactly right
that one slow sample must not define the whole result, and the
inverse is equally true: it must not be dismissed as a one-off without
the real n=15 distribution to check that against either. **Final
p50/p90 for both metrics, the real verdict on whether this ~12x gap
persists across a full run, and the actual per-metric colocation
conclusion all remain genuinely unwritten** — they belong in a future
15f update once the operator has actually run a complete, uncrashed
n=15 benchmark and, ideally, the diagnostic tool above against at
least one of its outliers. Reporting a verdict without that data would
be exactly the kind of unearned confidence this project's own
"verify, don't guess" standard has consistently rejected elsewhere.
**That data now exists — see 15f directly below.**

### 15f — the real result: closing the entire step 14/15 arc

**A full n=15 run, on the real VPS, genuinely PUSH-based throughout.**
All 15 fires succeeded. PUSH confirmed on every arm — 30/30 push
confirmations across the run (both the arm-time "WS push path
established" log and each fire's `method="push"` result), **zero POLL
fallback**. This is the first time in this project's history gap #11
has actually been closed with evidence, not a "should work outside
this sandbox" caveat — every number below is a genuine measurement of
what this bot's own PUSH-based detection does on real infrastructure,
not something entangled with a poll interval the way every prior
benchmark (13f, 14b) necessarily was.

| Metric | Real n=15 PUSH result | MintDash (p50) | Ratio |
|---|---|---|---|
| send→ack p50 | 174ms | 117ms | ~1.5x |
| send→ack p90 | 235ms | — | — |
| dispatch→inclusion p50 | 2127ms | 136ms | ~15.6x |
| dispatch→inclusion p90 | 2367ms | — | — |

**Diagnostic confirmation, not just a number taken at face value** —
`deploy/lib/diagnose_inclusion_delay.py` run against a real mid-pack
fire from this run (tx `0x3667e4bd...`, bot-measured
`dispatch_to_inclusion_ms: 1888`): the tx's dispatch-time block was
`~105155922`, its actual inclusion block was `105155937` — **~15 real
blocks elapsed**, confirmed directly from on-chain block numbers, not
inferred. This rules out the tool's own "≤2 blocks means node/
subscription lag, not real delay" case entirely — the delay is
genuinely on-chain, not a detection artifact. The same tx's
`effectiveGasPrice` paid **zero priority fee above base fee** —
consistent with, not contradicted by, Robinhood Chain's documented
FCFS sequencing (gas price doesn't affect ordering there) — so this is
**confirmed not attributable to underpriced gas**; raising
`priority_fee_multiplier`/`max_priority_fee_gwei_cap` would not be
expected to fix it.

**The final verdict, per metric — this is the answer this whole step
existed to produce:**

- **send→ack (174ms vs. MintDash's 117ms, ~1.5x):** plausibly
  explained by RPC/network proximity — this run's testnet Alchemy
  endpoint from a `us-east-1` VPS vs. MintDash's own colocated node.
  **This is the number a future colocation/dedicated-node step could
  reasonably expect to move.**
- **dispatch→inclusion (2127ms vs. MintDash's 136ms, ~15.6x):**
  **CONFIRMED as real on-chain delay via direct block-number evidence**
  — not a measurement artifact, not a poll-interval confound (unlike
  every number in 13f/14b), not attributable to this bot's own
  detection method at all. **Also confirmed not attributable to gas
  pricing**, given this chain's documented FCFS model and the zero-
  priority-fee-paid evidence above. **RPC proximity/colocation would
  improve HOW FAST this bot learns about an inclusion that already
  happened — it has no bearing on WHEN inclusion itself happens on
  this specific chain's sequencing model.** These are two genuinely
  different problems. A colocation/dedicated-node step is well-
  supported for send→ack specifically; it would NOT be expected to
  move dispatch→inclusion at all, and proposing it as a fix for that
  number would be solving the wrong problem — this corrects 14b's own
  (already appropriately hedged, but now resolvable) open question,
  and supersedes any framing anywhere in this file that treated
  "colocation helps proximity-sensitive numbers" as applying to
  dispatch→inclusion broadly rather than to send→ack specifically.

**The real open question, stated plainly rather than guessed at:**
*why* Robinhood Chain testnet's sequencer takes ~15 blocks
(~3.4s at the measured 227ms block time) to include a transaction that
reached it, when the chain's own block-production cadence is far
faster, is genuinely unknown from this investigation. This session has
no ability to inspect Robinhood Chain's sequencer internals, no
mainnet data to compare testnet behavior against, and no scope to
investigate further here — worth its own dedicated investigation if
inclusion latency matters for a real drop (it likely does, for a
sniper), but explicitly out of scope for this write-up. Don't let the
"confirmed real, confirmed not gas, confirmed not RPC lag" findings
above be mistaken for "fully explained" — two real candidate causes
(gas pricing, RPC/detection lag) were ruled out with evidence; the
actual cause was not identified.

**Step 14b's HTTP-poll-confounded numbers (978ms/1329ms p50/p90 on
Robinhood testnet) are now explicitly superseded by the real PUSH
numbers above** — not deleted, kept as the historical record of the
investigation that correctly diagnosed its own limitation and scoped
exactly what evidence would be needed to resolve it, which this step
finally provides. Step 13f's original n=1 attempt (7551ms) remains
superseded by 14b as before. **Gap #11 is closed, for real, with
evidence — the first time that has ever been true in this project.**

### 15g — a new, separate open question: real sequencer delay, or
Alchemy-specific indexing lag? (does NOT change 15f's verdict above)

15f's diagnostic confirmed real on-chain delay at the transaction-
receipt level — genuine, not in question. What it could NOT distinguish
is a more precise variant of the RPC-lag hypothesis: whether the
Robinhood Chain *sequencer* actually produced the including block
significantly earlier than *Alchemy's own node* indexed and served it
as queryable — which would look identical to real sequencer delay from
this bot's receipt-level view, while the underlying chain may have
actually been faster. Investigated as far as this session honestly
could; **inconclusive, and 15f's verdict above is unchanged by this
section** — read on for exactly why, and what would actually settle it.

**Protocol confirmed directly, not assumed — this was the first thing
checked, per this project's own standard.** Robinhood's node-operator
docs (`docs.robinhood.com/chain/run-a-full-node/`) and a third-party
decoder project built specifically for this feed
(`chainstacklabs/robinhood-chain-sequencer-feed`, whose own description
is "Offchain Labs' Nitro relay for transport, a fast lazy decoder for
everything after") both confirm: `wss://feed.testnet.chain.robinhood.com`
speaks Arbitrum Nitro's own sequencer-feed relay protocol, **not**
standard `eth_subscribe` JSON-RPC. It is not a drop-in swap for
`alloy`'s existing `WsConnect`/`subscribe_blocks()`, which only speaks
standard Ethereum pubsub — a real, code-relevant fact, confirmed before
attempting anything, not assumed from the URL's `wss://` scheme alone.

**Connected to it anyway, from this sandbox — and it worked, at the
protocol level, which was itself unexpected.** Unlike every prior
`alloy`/`rustls`-based WS attempt in this project's history (gap #11 —
blocked by this sandbox's TLS-interception proxy, since `alloy`'s
`webpki-roots` trust store doesn't trust that proxy's CA), a plain
Python `websockets` client completed the TLS handshake and received
real, well-formed JSON messages — `python3`'s TLS stack evidently trusts
whatever CA this sandbox's proxy presents, where `rustls`'s
hard-compiled trust store does not. Real message shape, captured live
(not fabricated): `{"version":1,"messages":[{"sequenceNumber":N,
"message":{"message":{"header":{"kind":3,"blockNumber":B,
"timestamp":T,...},"l2Msg":"<base64>"},...},"blockHash":"0x...",
"signatureV2":"...",...}]}`.

**But the data received was frozen, not live — a concrete, evidenced
finding, not a guess.** Three fully independent connection attempts,
spread across real, separate process invocations several minutes
apart, all returned a **byte-for-byte identical first message**
(`sequenceNumber=105316976`, `header.blockNumber=11540856`,
`header.timestamp=1787375057` — every single time). Draining 60
consecutive messages from one connection advanced `sequenceNumber` by
exactly 60 (one per message, real progress within the stream) but
`header.blockNumber` never moved past its very first value, and the
message timestamps stayed ~800+ seconds stale the entire time,
regardless of real elapsed wall-clock time between attempts. This
reads as this sandbox's own outbound proxy caching or otherwise not
passing through genuinely live traffic for this specific long-lived
WS endpoint — a different, more insidious manifestation of gap #11
than an outright connection failure (a connection that *looks*
successful but silently serves stale data is worse to build on than
one that visibly fails), and worth flagging precisely for that reason.

**A separate, real ambiguity surfaced along the way, unresolved:**
`sequenceNumber` (~105.3M, matching the RPC's `eth_blockNumber` order
of magnitude) and `header.blockNumber` (~11.5M, a completely different
and far-slower-moving range) cannot both be "the L2 block number" —
one of them is something else, likely an internal Nitro concept (an L1
reference index, a delayed-inbox counter, or similar) distinct from
the standard block height `eth_getBlockByNumber` exposes. This
session's own frozen-data problem prevented resolving which is which
with a live cross-check; the operator-run test below includes exactly
how to settle it in passing.

**Genuinely inconclusive from this sandbox — stated plainly, not
stretched into a conclusion either direction.** Neither "real sequencer
delay" nor "Alchemy-specific indexing lag" is confirmed or ruled out by
anything captured here. 15f's own finding — that the ~15-block delay is
real (not a bot-side detection artifact) and not gas-price-related —
stands entirely unchanged; this section only narrows what "real" could
still mean underneath that.

**The exact operator-run test that would settle it, since this session
cannot:**
```bash
# On the real VPS, at the moment of (or right after) a benchmark fire:
# 1. Connect to the sequencer feed and capture ONE live message's
#    sequenceNumber, header.blockNumber, header.timestamp, and blockHash.
# 2. Immediately query the SAME Alchemy endpoint config.toml already
#    uses for both candidate numbers, to resolve which one is the real
#    L2 block height:
curl -sS -X POST "$ALCHEMY_HTTP_URL" -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["0x<sequenceNumber_hex>",false]}'
curl -sS -X POST "$ALCHEMY_HTTP_URL" -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["0x<header_blockNumber_hex>",false]}'
# Whichever call's returned "hash" field matches the feed message's own
# "blockHash" is the real L2 block number in the standard numbering
# space. THEN compare that RPC block's "timestamp" against the feed
# message's own header.timestamp for the SAME block, captured as close
# in wall-clock time as practical. A material, consistent gap (Alchemy
# reporting a LATER timestamp than the feed for the same block)
# supports Alchemy-specific indexing lag; matching timestamps rule it
# out and point back to genuine sequencer-side delay as 15f already
# found.
```
This needs a real VPS whose outbound network isn't behind this
sandbox's proxy (the stale-data problem above should not recur there),
and the operator's own configured Alchemy endpoint (never available to
this session). Worth running if inclusion latency matters enough for a
real drop to justify chasing further — genuinely open, not urgent
enough on its own to block anything already shipped.

### Step 19 — the original single-fire test's three tx, finally
diagnosed directly (they never had been until now)

15f's diagnostic ran against one representative tx from the n=15
batch. The **original** three single-fire-test numbers this whole
arc started from — `dispatch_to_inclusion_ms` **2722 / 2876 / 3023**,
first reported earlier in this file — had never actually been run
through `diagnose_inclusion_delay.py` themselves; that section
explicitly said so ("this session still cannot reach the real chain
or a real fire's tx hash"). Read-only, public-data check, done now:

- **RPC used:** `https://rpc.testnet.chain.robinhood.com` — Robinhood
  Chain's own public testnet endpoint, found directly in
  `docs.robinhood.com/chain/connecting` (not reused from the
  operator's Alchemy key, and not assumed from memory). Verified live
  before use: `eth_chainId` → `0xb626`, `eth_blockNumber` → a current,
  advancing block height. One wrinkle worth recording for next time:
  this endpoint 403s Python's default `urllib` User-Agent (Cloudflare
  in front of it) — plain `curl` and a spoofed `User-Agent` both work
  fine; not a sign the endpoint itself is unhealthy.
- **All three tx confirmed successful** (`status=success`), each with
  `effectiveGasPrice == baseFeePerGas` — **zero priority fee paid**,
  same as 15f's own tx — consistent with Robinhood Chain's documented
  FCFS sequencing, not a new anomaly.

| tx | logged dispatch_to_inclusion_ms | inclusion block | blocks elapsed |
|---|---|---|---|
| `0x6487f122...b4817` | 2722 | 105139097 | **21** |
| `0xc88a41f7...6240b7` | 2876 | 105139098 | **22** |
| `0x481d30b1...e4b8e54` | 3023 | 105139096 | **28** |

All three inclusion blocks land within 2 of each other, consistent
with these being the same single arm's near-simultaneous wallets, and
all three show double-digit `blocks_elapsed` — nowhere near the tool's
own `<=2` node/detection-lag threshold. **This confirms the existing
write-up, not just as an isolated n=1 confound anymore:** two
independent samples (this file's original n=1/three-wallet fire, and
15f's separate n=15 run), against two independent RPC providers
(Robinhood's own public endpoint here vs. Alchemy in 15f), both show
tens of real blocks elapsed with zero priority fee paid. That a
completely different, non-Alchemy RPC reproduces the same pattern is
itself a small but real additional data point against the still-open
15g "Alchemy-specific indexing lag" hypothesis specifically for these
three tx — it does not settle 15g's broader question (this was a
receipt/RPC-node check, not the sequencer-feed-vs-Alchemy-timestamp
cross-check 15g's own operator-run test describes), but it is one
more independent RPC agreeing with Alchemy's numbers rather than
disagreeing with them.

No code changes were made or needed — this was a verification of
existing evidence, not a new finding requiring a fix.

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

**CORRECTION, added after further live debugging the same night — read
this before anything else in this section.** The actual, confirmed root
cause of that night's live crash loop was **not** the alloy `WsConnect`
behavior described below. It was a plain, mundane config mistake: an
unedited placeholder value left in `http_rpc_urls`
(`"https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"` — the literal
`YOUR_KEY` string from `config.example.toml`, never swapped in when
`ws_rpc_url` was correctly updated for Robinhood Chain). Alchemy quite
reasonably rejected that literal, unedited key, and its rejection
produces the exact same `error code -32600: Must be authenticated!` text
that the alloy userinfo-extraction bug below would *also* produce — two
unrelated causes, one identical-looking symptom. That's the actual reason
this took two full investigation rounds of alloy source-diving plus live
debugging on the real VPS to resolve, instead of being caught in minutes:
the wrong root cause was chased first, based on a plausible-sounding but
ultimately wrong theory, because it was investigated before the simpler
possibility was ruled out.

**Process lesson for future sessions, stated explicitly so this isn't
relearned live at 2am again:** when an error message is generic enough to
plausibly come from more than one unrelated cause — as
"-32600 Must be authenticated!" was here, matching both a stale/wrong RPC
URL value and a URL-parsing/auth-header bug — check the simplest,
most-recently-touched possibility FIRST (a config field that was hand-
edited earlier in the same session, in this case) before escalating to a
deeper code investigation. Reading `config.toml` end to end for anything
that still says `YOUR_KEY`, `CHANGE_ME`, or any other example/placeholder
text — a 30-second check — would have found this immediately. It was
found live on the VPS only after the investigation below had already run
its course.

**The alloy `WsConnect` finding below is still real and still correctly
fixed — it just isn't what broke this specific deploy.** It's a genuine,
source-confirmed behavior of `alloy-transport-ws` that this codebase
never wants triggered, so `Config::validate()`'s rejection of embedded
URL credentials and the redacted-URL error wrapping on all four
`connect_ws` call sites (both described below) are staying in the
codebase as legitimate hardening — a latent bug worth having closed,
independent of whether it caused this particular incident. Likewise the
`bus::log`-invisible-to-`journalctl` fix in the step 17 follow-up further
below is a real, independently-confirmed observability gap, unaffected by
this correction.

**Symptom, live-confirmed on the real deployed VPS:** the bot crash-looped
at boot with `server returned an error response: error code -32600: Must
be authenticated!` from Alchemy, on the exact same `wss://` URL and key
that both a plain HTTP curl AND a bare Node.js `ws` client (sending
`eth_chainId`) succeeded against. This ruled out the credential/Alchemy
app itself — something specific to how *this codebase* opens the
connection differed from a bare WS client in a way Alchemy's endpoint
read as an auth attempt gone wrong. (As the correction above explains,
this framing — comparing the bot's WS connection to a bare client's WS
connection — was itself pointed in the wrong direction: the actual
failing request that night was an HTTP call using the stale
`http_rpc_urls` placeholder, not the WS connection at all. Left as
originally written below for the historical record of what was
investigated and why, not as a statement of what actually happened.)

**Alloy-side finding, confirmed by reading alloy 2.4.1's actual source
(not assumed from memory) — a real, worth-fixing behavior, NOT this
incident's cause (see correction above):**
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

**At the time this was written, this session could not confirm the
operator's real `config.toml` was actually corrupted this way — it
subsequently was not** (see the correction at the top of this section:
the real cause was a stale placeholder in `http_rpc_urls`, found later by
live debugging on the actual VPS). The fix below stands anyway, as
legitimate hardening against a real behavior confirmed directly in
alloy's source — it closes a latent bug this codebase never wants
triggered, independent of whether it caused this specific incident. What
shipped:

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
the exact same unwrapped error anyway.** (In hindsight, this was
inevitable and unrelated to any gap in the WS fix itself — per the
correction at the top of this section, the WS code was never what was
failing that night, so no amount of hardening it further was going to
change the error the operator saw. But the investigation this follow-up
round did — confirming no 5th WS call site exists, and finding the
separate `bus::log`-invisible-to-`journalctl` gap below — is real and
independently worth having, regardless of the wrong initial premise.)
This round's original framing was "which boot-path call site was
missed" — the honest answer turned out to be none. An exhaustive re-grep
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

**Historical operator-verification steps below, left as originally
written for the record of what was actually asked and tried, superseded
by the correction at the top of this section.** The real fix for that
night's incident was simply editing `http_rpc_urls` to replace the
literal `YOUR_KEY` placeholder with the operator's real Alchemy key,
found and applied directly on the VPS through live debugging, not through
anything shipped in a commit — there was no code or config-shape bug to
patch for the actual cause, since a correctly-filled-in placeholder needs
no validation to catch (an *unedited* placeholder is, definitionally,
syntactically valid config the way a real key is too — `Config::validate`
has no way to distinguish "a URL with a plausible-looking key" from "a
URL with the literal example key still in it"). The commands below were
this session's best-effort verification guidance at the time, written
before the real cause was known:
```
cd /opt/mint-sniper/repo && sudo -u mint-sniper git pull --ff-only origin main
sudo -u mint-sniper -H bash -c "source \$HOME/.cargo/env && cd /opt/mint-sniper/repo/mint-sniper && cargo build --release"
sudo systemctl restart mint-sniper
sudo systemctl status mint-sniper --no-pager   # expect: active (running), not a crash loop
sudo journalctl -u mint-sniper -n 50 --no-pager  # the step 17 follow-up's tracing::error! fix (below) means this
                                                  # now actually shows watcher/copymint errors, where before it
                                                  # would have shown nothing from those two paths regardless
```
**The lesson that actually resolved this** — stated in the correction at
the top of this section, repeated here since it's the part worth a future
operator or session actually internalizing: before chasing a WS-connect
code theory for an ambiguous auth-flavored RPC error, read `config.toml`
end to end for any field that still says `YOUR_KEY` or other
`config.example.toml` placeholder text. It is the fastest possible check
and would have found this in well under a minute.

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
- **Step 21c — CI green is not the finish line either; the commit has to
  actually reach `main`, or it's still invisible to VPS redeploys.**
  Incident: at least three separate pushes to
  `claude/mint-sniper-audit-port-oxom5x` (steps 15f, 15g, 19) each went
  green on real per-job CI and were each reported "done" — but none of
  them were ever merged into `main`, since a feature-branch push alone
  never does that. This sat silently blocking the operator's VPS
  redeploys (which track `main`) until the operator noticed and opened
  PR #10 by hand. The previous convention above only closed the "did CI
  actually run" half of the gap; it said nothing about "did the work
  become reachable from `main`," which is the half that actually matters
  for deploys. **The fix:** this session's GitHub access is the repo
  owner's own token (confirmed via `get_me`) — it can open, update, and
  merge PRs on this repo. Going forward, for this session's OWN work on
  `claude/mint-sniper-audit-port-oxom5x` (or any future non-operator
  working branch), auto-merge into `main` is the default once real
  per-job CI is confirmed green on the head commit — open (or update) a
  PR, confirm CI, merge it, then confirm with `git merge-base
  --is-ancestor <sha> origin/main` (not just trusting the merge API's
  response) that the commit is actually reachable from `main` before
  calling the step done. **Standing exception, never overridden by the
  above:** the operator's own personal feature branches (e.g.
  `p0-rh-race-jitter-sequencer` / PR #9) are never auto-merged under any
  circumstance — those stay the operator's own merge decision, exactly
  as already established. Closed for real here: PR #10 (steps 15f/15g/19)
  merged to `main` as commit `f06e937`, confirmed reachable via the same
  ancestor check this convention now requires.

## Step 20 — EIP-7702 delegation/batching: research and design, no implementation

Same discipline as step 10a's identity research: confirm real facts before
writing any code. This expands the codebase's trust surface more than
anything built so far, including identity — a malicious or buggy delegate
contract has **full authority over a wallet**, not read access or a scoped
permission. No code changes in this step.

### 20a — the actual mechanism, confirmed not assumed

**alloy 2.4.1 (already in use throughout this codebase) supports EIP-7702
natively.** Confirmed directly against the vendored crate source, not
assumed from the EIP being live: `alloy-consensus-2.4.1/src/transaction/
eip7702.rs` defines `TxEip7702` (the type-0x04 `SetCodeTransaction`) with a
real `authorization_list: Vec<SignedAuthorization>` field, part of the
standard transaction envelope alongside the existing EIP-1559 type this
codebase already signs and sends. Signing/sending support exists; nothing
about alloy itself blocks building this.

**The architecture question, answered — this determines the whole scope:**
- EIP-7702 ALONE gives one thing: an EOA can point its own code at a
  deployed contract, so *that specific EOA* starts executing arbitrary
  delegate logic instead of nothing. It does NOT by itself give "one
  operator wallet pays gas for N sniper wallets" — that requires the
  delegate contract to expose sponsor/relayer execution logic on top of
  the base EIP (someone else's tx invokes the delegate, the delegate acts
  on the now-smart EOA's behalf), or pairing with ERC-4337's paymaster
  mechanism once the EOA has become a smart account via 7702.
- Real BATCHING (combining multiple wallets' operations into fewer
  transactions) is the same story: 7702 alone lets a single delegated EOA
  batch its OWN multiple calls into one transaction (useful, but not what
  "N sniper wallets, fewer txs" means). Batching *across* wallets needs
  ERC-4337-style infrastructure (a bundler/EntryPoint) layered on top —
  every source describes 7702 and 4337 as complementary, not either/or:
  "the future of account abstraction likely involves both standards
  working in harmony." **This is a two-layer feature, not a one-EIP
  feature** — 7702 alone does not give this codebase gas-sponsorship or
  cross-wallet batching; it's the enabling primitive ERC-4337
  infrastructure would sit on top of.

### 20b — security research (the part that matters most)

**Real, large-scale, ongoing attacks — this is not a theoretical risk.**
Since Pectra activated EIP-7702 on Ethereum mainnet (May 7, 2025):
- Within four weeks, Wintermute's research team found **more than 97% of
  EIP-7702 delegations on mainnet pointed to copy-pasted sweeper
  contracts**, the largest family dubbed "CrimeEnjoyor."
- Independent security research links **63% of analyzed EIP-7702
  authorization transactions to attacker-controlled contracts**, with
  **more than $2.3 million in confirmed thefts** identified.
- By late August 2026, a single user lost **$1.54 million** in one 7702
  batch-transaction phishing attack.
- Attack mechanism: a victim signs ONE authorization tuple (can look
  structurally harmless), which installs delegate code with **persistent,
  unconditional execution control** over the EOA — categorically different
  from phishing a single transaction, since every subsequent action routes
  through attacker logic with no further victim signature required.

**Verdict for this codebase specifically:** the dominant real-world outcome
of EIP-7702 adoption so far is mass phishing against end users signing
authorizations toward attacker contracts they didn't audit. That is a
different threat model from this project's own wallets (private keys held
server-side, never end-user-signed in a browser) — but it establishes that
**the mechanism itself is proven dangerous when the delegate target is
wrong**, which is exactly the risk of writing a custom delegate contract
in-house versus reusing something already battle-tested.

**Audited, reusable delegate implementations exist and should be strongly
preferred over a custom one.** OpenZeppelin publishes `EOA Delegation`
documentation and reference contracts as part of their audited 5.x
contracts library. MetaMask's Delegation Toolkit works with any ERC-4337
bundler/paymaster. Safe{Wallet}'s own delegate pattern is referenced as the
"become a smart account" path multiple sources point to. **A
custom-written delegate contract holding full authority over this
project's sniper wallets is a categorically larger, more dangerous attack
surface than reusing one of these** — if this is ever built, reuse is not
optional-but-preferred, it's close to a hard requirement given the proven
blast radius of getting the delegate contract wrong.

**Revocation — real, documented, RUNBOOK.md-worthy procedure exists.**
Setting the authorization's target address to the zero address
(`0x0000000000000000000000000000000000000000`) is the canonical
revocation: sign a fresh authorization (using the EOA's own still-held
private key — the EOA is never dispossessed of its key by delegating) with
the zero address as target, broadcast it. `cast wallet sign-auth
--private-key <KEY> --chain <ID> --nonce <NONCE> 0x0000...0000` is the
concrete CLI form; libraries like Candide's AbstractionKit expose the same
as `createRevokeDelegationTransaction`. There is no permanent migration —
this genuinely returns the EOA to a normal, undelegated account. **This is
the RUNBOOK.md entry this feature would need before shipping**: if a
delegate is later found compromised or buggy, the operator's procedure is
"sign and broadcast a zero-address authorization from the affected wallet's
own key" — cheap (one small tx per wallet), fast, and does not require
migrating funds to a new address.

### 20c — compatibility check against existing invariants

- **`advance_nonces` / the prepare/fire split's nonce assumptions (step
  3b):** EIP-7702 does not introduce a separate or parallel nonce space —
  the delegating EOA's existing account nonce continues incrementing
  normally, and signing an authorization itself consumes one nonce slot
  (same sequential-nonce rule as any transaction). This does not BREAK the
  existing model (still one linear nonce counter per EOA, still
  client-side tracked, still the same "advance exactly once, at the moment
  of commit" rule step 3b established) — it would just mean a wallet's
  first-ever authorization tx is one more nonce-consuming event
  `next_nonce` tracking needs to account for, the same way any other
  transaction from that wallet already is. Genuinely orthogonal to the
  actual bug class step 3b fixed (premature nonce advancement on a
  never-broadcast prepare) — nothing about 7702 reintroduces that risk.
- **Custody discussion (steps 11/12 — operators eventually bringing their
  own keys, session-scoped custody):** this is a real, direct connection,
  not orthogonal, and worth flagging explicitly. If wallets are ever
  delegated to a shared contract, "who controls this wallet" stops being
  a single fact (the private key holder) and becomes at least two facts
  (the key holder AND whoever controls/can upgrade the delegate
  implementation). Any future custody model that lets an operator bring
  their own key needs to also account for whether that operator's wallet
  is delegated, and to what — a session-scoped custody boundary that
  doesn't also scope delegate-contract trust would leave a gap. This
  should be resolved in the SAME architecture conversation as steps 11/12,
  not decided independently and bolted on afterward.

### 20d — real options, not a recommendation

1. **No 7702, status quo.** Keeps the current, well-understood surface
   (server-held keys, `advance_nonces`, prepare/fire split — nothing about
   wallet authority changes). Buys nothing new. Zero new attack surface,
   zero new implementation cost. The only cost is *not* getting
   gas-consolidation or batching, if either turns out to matter enough to
   justify the alternative below.
2. **7702 for gas-consolidation only, reusing an audited delegate contract**
   (OpenZeppelin's or an equivalent, never custom-written). Buys: one
   operator-funded wallet can cover gas for N sniper wallets without
   pre-funding each one individually — a real operational simplification
   for wallet management. New attack surface: the reused delegate
   contract's own trust assumptions become this project's trust
   assumptions (bounded by whatever audit/track record it has, not
   unbounded); the revocation procedure from 20b becomes a real
   operational dependency (RUNBOOK.md entry, tested at least once).
   Implementation cost: moderate — new `authorization_list` construction
   and signing path in executor.rs, a decision on which audited delegate
   to target, real testnet dry-run before trusting it for a live mint,
   same standard as every other feature in this project's history.
3. **7702 + ERC-4337 for full batching.** Buys the most: genuine
   cross-wallet batching (fewer total transactions across N wallets), the
   closest match to the 100-wallet/16-second/near-zero-cost shape 20e
   below describes. New attack surface is the largest of the three: a
   bundler/paymaster dependency added on top of the delegate-contract
   trust from option 2, meaning two new pieces of external infrastructure
   this project would depend on, not one. Implementation cost is the
   highest — this is a genuinely new subsystem, not an incremental change
   to `fire_prepared`.

This is the operator's decision, not a recommendation — consistent with
how the custody-model question in step 12 and the WebAuthn-origin decision
in step 10.5a were both left as explicit choices, not defaults.

### 20e — the 100-wallet / 16-second / 0.0006 ETH data point

A competitor's own execution history reportedly shows: 97/100 wallets
included, 16 seconds total, 0.0006 ETH total gas across all 100 wallets —
averaging **~0.000006 ETH (~$0.01–0.02) per wallet**. Whether this ran on
Ethereum mainnet is explicitly **not confirmed** from what's available to
this session (a screenshot detail, not independently verifiable) — treated
as an open question, not a fact, throughout this analysis.

**Two questions, answered as plainly as the evidence allows:**

1. **Is this per-wallet cost achievable via EIP-7702 specifically on a
   real-gas-cost chain, or is a near-zero-gas L2 the simpler explanation?**
   A single ERC-721-style mint transaction on Ethereum mainnet typically
   costs on the order of 21,000 (base) + 50,000–150,000+ (mint logic) gas
   — even at a low 1 gwei gas price, that's roughly 0.00007–0.00017 ETH for
   ONE wallet's mint alone. 100 independent mints at that rate would cost
   on the order of 0.007–0.017 ETH total, not 0.0006 ETH — **the claimed
   total is 1–2 orders of magnitude too low for 100 real, independently-
   priced mainnet mint transactions**, with or without EIP-7702 in the
   picture (7702 authorizations themselves cost real gas too — one model
   cited ~35,190 gas per new delegation indicator, which is additive on
   top of the mint cost, not a discount on it). A near-zero-gas L2 (this
   project's own Robinhood Chain benchmarks show baseFeePerGas on the
   order of 0.01 gwei — four to five orders of magnitude below typical
   mainnet gas prices) fits this number far more simply, with no
   delegation mechanism of any kind required to explain it.
2. **If 7702 genuinely could produce this on mainnet, how?** No credible
   mechanism was found. EIP-7702 authorizations do not share or amortize
   execution gas cost across delegated accounts — each mint is still a
   full, independently-metered EVM execution; the delegation only changes
   WHO can author calls FROM that account, not what those calls cost to
   execute. An operator wallet "sponsoring" gas via a relayer pattern
   changes who PAYS, not the total amount paid — 100 independent mints
   still cost the sum of 100 independent mints' worth of gas, sponsor or
   no sponsor. Nothing in EIP-7702's own mechanism reduces per-transaction
   execution cost.

**Verdict, stated without hedging where the evidence supports one:** the
evidence strongly favors explanation 2 — a near-zero-gas chain (most
plausibly Robinhood Chain, an L2 this project already benchmarks against),
not EIP-7702 delegation, explains this number. This is **not** a
numbers-backed case for building EIP-7702 on Ethereum mainnet — if
anything, it's a data point that a chain choice (already-supported
Robinhood Chain, or a similarly cheap L2) does far more for per-wallet
cost than any delegation mechanism could, on mainnet or otherwise. Flagged
as ambiguous only insofar as the exact chain is unconfirmed; the magnitude
argument against "this is what 7702 buys you on mainnet" is not ambiguous.

Sources consulted (WebSearch, current as of this writing): arXiv "EIP-7702
Phishing Attack" (2512.12174); TradingView/NewsBTC/Cryptopolitan coverage
of Wintermute's CrimeEnjoyor research and the $1.54M single-user loss;
OpenZeppelin's EOA Delegation docs; BuildBear's ERC-4337-vs-EIP-7702
comparison; Ethereum.org's Pectra/7702 guidelines; EIP-7702's own spec
(eips.ethereum.org/EIPS/eip-7702) for the revocation mechanism.

## Step 22 — InkChain support

Same rigor as step 13's Robinhood Chain work: verify before building, no
assumption carried over just because the pattern is familiar. All facts
below are from real, live checks run this session (RPC calls, direct
docs fetches), not memory.

### 22a — SeaDrop deployment, confirmed via real eth_getCode

- **InkChain mainnet:** `eth_getCode` against
  `0x00005EA00Ac477B1030CE78506496e8C2dE24bf5` (the SeaDrop V1 singleton
  address this codebase already targets on Ethereum/Robinhood Chain)
  returned real, non-empty deployed bytecode. **Confirmed deployed**, same
  address as every other chain checked so far — the cross-chain
  deterministic-address theory holds here, but it was verified, not
  assumed.
- **InkChain testnet (Ink Sepolia):** the SAME `eth_getCode` call against
  the SAME address returned `0x` — **empty, not deployed there.** The
  singleton does not hold everywhere; testnet is the exception found by
  actually checking rather than assuming. No alternate InkChain-testnet
  SeaDrop address was found in the time available for this step — this is
  an explicit, stated gap (see 22d), not silently worked around.

### 22b — real chain facts

- **Chain IDs, confirmed from InkChain's own docs
  (docs.inkonchain.com/general/network-information), not memory:**
  mainnet **57073**, testnet (Ink Sepolia) **763373**. Cross-checked live:
  `eth_chainId` against the mainnet RPC returned `0xdef1` (57073) and
  against testnet RPC returned `0xba5ed` (763373) — docs and live
  responses agree.
- **RPC provider support:** Alchemy supports InkChain mainnet
  (`ink-mainnet.g.alchemy.com`), same provider pattern already used for
  Ethereum and Robinhood Chain — no new provider integration needed.
  InkChain's own docs also publish public (rate-limited) endpoints:
  `https://rpc-gel.inkonchain.com` (mainnet) / `https://rpc-gel-
  sepolia.inkonchain.com` (testnet), both confirmed live and responsive
  this session.
- **Real, measured block time** (same methodology as step 14b — walking
  real consecutive block timestamps, not trusting a documented figure):
  50 consecutive InkChain mainnet blocks, timestamp deltas **every single
  one exactly 1 second** — a consistent, real **1000ms** block time.
  InkChain's own docs do not publish a block-time figure at all (checked
  directly — the "note" is real, not a gap in this session's search), so
  there was no documented number to compare this measurement against the
  way Robinhood Chain's ~100ms-documented-vs-~227ms-measured mismatch
  existed. Same 1-second-timestamp-granularity caveat as every prior
  measurement in this file applies (true value could differ from exactly
  1000ms within that resolution).
- **FCFS-vs-gas-priority sequencing:** InkChain is built on the OP Stack,
  with Kraken operating as its sequencer. OP Stack sequencers generally
  default to first-come-first-served ordering by arrival time at the
  sequencer, the same qualitative model already confirmed for Robinhood
  Chain — **but this is the general OP Stack pattern, not a statement
  independently confirmed from InkChain's own docs the way Robinhood's
  FCFS claim was step 13's own direct finding.** Treat as
  likely-but-not-independently-verified for InkChain specifically; worth
  a direct confirmation (an InkChain support/docs question, or empirical
  testing) before designing race_mode-style behavior around it the way
  step 23 discusses.
- **Mempool/pending-transaction visibility — real, negative finding.**
  Directly probed InkChain's public mainnet RPC:
  - `eth_newPendingTransactionFilter` → `{"code":-32601,"message":"rpc
    method is not whitelisted"}`
  - `txpool_status` → same "not whitelisted" rejection
  - A raw WS `eth_subscribe("newPendingTransactions")` attempt against
    `wss://rpc-gel.inkonchain.com` was rejected outright at the
    connection layer (`HTTP 405`), before ever reaching subscription
    logic.
  - By contrast, `eth_getBlockByNumber("pending", false)` DID succeed and
    returned a real, non-empty "pending block" object (with one
    transaction hash in it) — a different, more limited kind of
    pre-confirmation visibility than a true pending-tx subscription,
    worth noting rather than flattening into a blanket "no visibility at
    all."
  
  **This directly confirms the task's own prediction**: mempool
  visibility is chain-specific, not guaranteed by EVM-compatibility
  alone. Copymint's existing detection mechanism (`copymint.rs`'s
  `subscribe_full_pending_transactions`, the same call
  `watcher.rs::run_mempool_watcher` uses for `trigger_mode =
  "mempool_watch"`) is **not confirmed usable on InkChain** via the
  public RPC checked here. A private/paid RPC provider (Alchemy or
  similar) *might* expose full pending-tx visibility where the public
  endpoint doesn't — genuinely unconfirmed either way from this session,
  not assumed to be better just because it's a paid tier. **Until
  verified against a real private endpoint, InkChain should be treated as
  `poll_state`/`timestamp`-mode-only** — this is the direct gate step 23
  needed and now has.

### 22c — configurable network, chain-agnostic re-verified

- **`chain_id` is read live, not hardcoded — re-confirmed for a second
  chain, not just assumed to still hold from step 13b's Robinhood Chain
  finding.** Grepped `executor.rs`/`config.rs` directly: `let chain_id =
  reader.get_chain_id().await...` in `prepare_fire`, used once via
  `tx.chain_id = Some(chain_id)` — a fresh RPC read every prepare, no
  chain ID constant anywhere in the signing/firing path. This genuinely
  is chain-agnostic infrastructure, not something that happened to work
  for Robinhood Chain specifically.
- **`config.example.toml` updated** with a documented InkChain example
  section (mainnet + testnet RPC URLs via Alchemy, the measured
  `block_time_ms = 1000`, and an explicit note on the testnet SeaDrop gap
  and the mempool-visibility finding above), matching the existing
  Robinhood Chain section's style exactly.

### 22d — testnet dry run: explicit gap, not skipped silently

**No live dry run was performed.** Two blockers, both real and confirmed,
not assumed:
1. SeaDrop is not deployed at the standard singleton address on InkChain
   testnet (22a) — no known live SeaDrop-based collection to target was
   found there in the time available for this step.
2. This session has no VPS access and cannot deploy a dedicated
   benchmark-only token the way step 14b's live operator-run dry run did
   for Robinhood Chain testnet.

**This is a stated, explicit gap**, same as this file's standing
convention for anything not actually verified end-to-end: InkChain support
as landed here is chain-configuration and fact-verification only (22a-22c)
— a real live-fire dry run (finding or deploying a real testnet SeaDrop
target, confirming a real mint succeeds, confirming real
`dispatch_to_inclusion_ms`/`send_to_ack_ms` numbers) has NOT happened and
should not be assumed to work the same way step 5's Sepolia dry run or
step 13d's Robinhood Chain dry run did, until it actually runs once.

cargo build/check/test clean (no functional Rust changes in this step —
`chain_id`-live-read was re-verified, not modified; only
`config.example.toml` changed).

## Step 23 — copymint front-running: research and design, no implementation

Scope: Robinhood Chain, Ethereum mainnet, InkChain. Uses step 22's actual
findings on InkChain below, not re-derived or guessed. No code changes in
this step.

### 23a — what copymint actually does right now, read directly from the code

**Copymint does NOT wait for the tracked wallet's transaction to confirm
before acting — this contradicts how it was described in prior
conversation.** Read `copymint.rs::handle_candidate` directly: it fires the
instant a tracked wallet's `mintPublic` call is seen **pending** (via
`subscribe_full_pending_transactions`, before any confirmation), does one
fresh `getPublicDrop` read to independently verify the drop is real and
currently live (never trusting the pending tx's own calldata beyond which
contract/fee-recipient it names), and — if `should_auto_fire` allows it —
immediately sends `ControlMsg::FireCopymint`. There is no wait, no
polling for the tracked wallet's own receipt, nothing that reads "was
their mint the one that landed" anywhere in this path.

**This means an emergent race already exists today, independent of
anything step 23 would add.** The tracked wallet's original transaction
and copymint's own generated transaction are both in flight simultaneously
the moment copymint decides to fire — whichever lands first is whichever
the underlying chain's ordering happens to favor, with zero deliberate
influence from this codebase over which one wins. **"Add front-running" is
therefore not new behavior in the sense of creating a race that doesn't
currently exist — it would be an optimization of a race copymint already
runs every single time it auto-fires**, specifically: deliberately trying
to win that already-existing race (via submission speed or gas bidding)
instead of leaving the outcome to chance. This reframes the actual
decision in front of the operator: not "should copymint start racing," but
"should copymint's already-existing race be actively won more often."

### 23b — per-chain mechanism

- **Robinhood Chain.** Confirmed FCFS, no gas-price priority (step 13's
  research, re-confirmed step 22b's InkChain check against the same
  question). "Front-running" here can only mean winning on raw submission
  speed to the sequencer — gas bidding has no effect on ordering by this
  chain's own documented design. **PR #9 (`p0-rh-race-jitter-sequencer`)
  is still open, not merged, as of this check** (confirmed live via the
  GitHub API, not assumed from memory) — but its `race_mode`/
  `sequencer_http_url` work already IS the right primitive: sequencer-
  first `eth_sendRawTransaction` submission, jitter disabled, is exactly
  "win on raw speed." Copymint would not need its own separate fast-path
  — it already calls the same `fire_prepared`/`ControlMsg::FireCopymint`
  path every other trigger mode uses (23a), so once PR #9 lands, copymint
  automatically inherits sequencer-racing behavior for free on this chain,
  no copymint-specific code required.
- **Ethereum mainnet.** Standard gas-priority ordering — a fundamentally
  different mechanism from Robinhood Chain's. Front-running here means
  reading the tracked wallet's PENDING transaction's actual fee fields
  (`maxFeePerGas`/`maxPriorityFeePerGas`, from the same pending-tx data
  copymint already decodes calldata from) and bidding above them —
  copymint currently decodes only calldata (contract/fee-recipient/
  quantity), never touches the pending tx's fee fields at all, so this is
  new decoding work, not a reuse of anything existing. **This is the case
  with real, direct stakes**: outbidding the tracked wallet on a
  tight-supply drop can cause THEIR transaction to revert (they lose the
  mint they were trying to make, not just a race copymint also entered).
  Copymint's existing `getPublicDrop` verification already fetches
  `maxTotalMintableByWallet`; extending it to also check real scarcity —
  `IERC721SeaDrop.getMintStats(address)` on the NFT contract itself
  (returns `minterNumMinted`, `currentTotalSupply`, `maxSupply` — a real,
  standard SeaDrop-adjacent function, confirmed to exist, not assumed) —
  is concretely buildable: `currentTotalSupply` vs `maxSupply` is exactly
  the scarcity signal 23c's gate needs, callable from the same RPC
  connection `fetch_public_drop` already uses.
- **InkChain.** Per step 22b's actual finding: the public RPC checked
  explicitly rejects `eth_newPendingTransactionFilter`/`txpool_status`
  ("not whitelisted"), and a raw WS pending-tx subscription was rejected
  at the connection layer entirely. **Copymint's detection mechanism —
  which structurally depends on subscribing to pending transactions —
  is not confirmed buildable on InkChain via the endpoint checked.**
  Stated plainly, not designed around: until a private/paid RPC endpoint
  is confirmed to expose full pending-tx visibility on InkChain (genuinely
  unverified, not assumed better just because it's paid), copymint should
  not be offered as a supported trigger on InkChain at all — only
  `poll_state`/`timestamp` modes, which don't need pending-tx visibility,
  are confirmed usable there.

### 23c — the safety/harm design (not optional)

**Scarcity gate, same spirit as the existing free/paid split (step 6c).**
Before any front-run attempt is even offered, let alone auto-fired: call
`getMintStats` on the target NFT contract, compute `remaining =
maxSupply - currentTotalSupply` and how close that is to zero relative to
the mint quantity in flight. Two zones:
- **Supply abundant relative to demand:** both the tracked wallet's mint
  and copymint's own mint are very likely to succeed regardless of who
  lands first — no real harm model exists here (nobody's mint gets pushed
  over a cap by one more transaction). Default-safe territory, the same
  category free copymint opportunities already occupy today.
- **Supply tight/near cap:** a real, non-hypothetical chance that winning
  the race causes the tracked wallet's own transaction to revert (their
  mint pushed past the now-exhausted cap by copymint's own action).
  **This must require the same tier of explicit, deliberate opt-in the
  existing paid-mint gate already uses** — never auto-fire, the operator
  must consciously enable front-running for this specific risk tier, not
  just see a warning label and proceed. A warning label is what search
  results already get (8c); this is a strictly higher-stakes action
  (deliberately risking causing someone else's transaction to fail) and
  needs the stronger gate, not the weaker one.

**Dedup-by-target check, designed as a real check, not a comment.** If two
different tracked wallets are both detected minting the SAME collection
within a short window, only act on the first; skip the second with a
clear, logged reason. Without this, copymint could fire twice on the same
drop from two independent triggers — wasted gas, wasted wallet allocation
on a redundant mint, and (once front-running exists) potentially two
separate front-run attempts against two different targets on the same
drop, compounding the scarcity risk above. Concrete design: an in-memory
(or `identity.db`-backed, for restart-survival — a decision for
implementation time) map of `nft_contract → last-fired timestamp`,
checked and updated atomically inside `handle_candidate` before any
`ControlMsg::FireCopymint` send, with a short TTL (long enough to cover
one realistic "two wallets both minting the same drop within seconds of
each other" window, short enough that a genuinely separate later drop on
the same contract — e.g. a second stage — isn't permanently blocked). A
competitor's own copy-mint tool implements exactly this pattern
("Copy mint skipped because this collection already has an active or
copied task"), independently validating the pattern is worth having, not
just this project's own idea.

### 23d — options, not a recommendation

1. **Network-speed racing only, no gas bidding, Robinhood-first.** Ship
   nothing copymint-specific for Robinhood Chain (23b: it already inherits
   PR #9's sequencer racing once that merges). Explicitly do NOT build the
   Ethereum gas-bidding path or the scarcity-gate infrastructure it
   requires. Buys: the lowest-risk win available — Robinhood Chain's own
   FCFS model means winning the race has no victim-revert harm model the
   way outbidding does on Ethereum, so this is close to free correctness
   improvement with none of 23c's hardest design work. Costs: does
   nothing for Ethereum mainlnet copymint opportunities, which stay
   exactly as risky/un-raced as they are today (i.e., today's existing
   emergent race continues, un-optimized, un-gated).
2. **Full gas-priority racing on Ethereum, with the scarcity gate as a
   hard requirement.** Ship both the Ethereum fee-bidding path (23b) and
   23c's full scarcity-gate + explicit-opt-in-for-tight-supply design
   together, as one unit — never ship gas-bidding without the gate. Buys:
   the highest-value copymint improvement (Ethereum mainnet has the
   biggest existing drop ecosystem this bot targets). Costs: the most
   implementation and review effort — new fee-field decoding, a new
   supply-check RPC call added to the hot detection path, a new
   confirmation-tier UI flow for the opt-in, and real testnet validation
   before trusting any of it live, given the direct "causes someone else's
   tx to revert" stakes.
3. **Defer Ethereum entirely until the scarcity-check infrastructure is
   proven on Robinhood Chain first.** Ship option 1 now; build and test
   the `getMintStats`-based scarcity check as its own standalone,
   low-stakes addition to `getPublicDrop`'s existing verification (surfaced
   in the UI, not yet gating anything) before ever pairing it with
   Ethereum gas-bidding. Buys: de-risks the hardest part (does the scarcity
   read actually work reliably, across real live drops, before it's ever
   load-bearing for a fire/no-fire decision) separately from the highest-
   stakes part (bidding real ETH against a real drop's real cap). Costs:
   slower path to Ethereum front-running than option 2, two separate
   review/testnet cycles instead of one.

Also decided as part of this scope, not deferred: **InkChain gets no
copymint front-running design at all in any of the three options above** —
per 23b, its detection mechanism itself is unconfirmed buildable there.
Whichever option is chosen, InkChain waits for that question to resolve
first, independent of the Robinhood-vs-Ethereum decision.

This is the operator's decision to make with full information, consistent
with how every other major architecture fork in this project (custody
model, WebAuthn origin, the EIP-7702 options in step 20 above) has been
handled — no option above is recommended over another.

## Step 24 — seadrop-noir-bot pattern evaluation (research only, no implementation)

Cloned `Kuriare7Rz/seadrop-noir-bot` to a scratch directory (same pattern
as step 1's audit of `morsyxbt/nft-public-mint`) and read the actual
source for four specific README claims — scope explicitly limited to its
legitimate on-chain SeaDrop fallback path, not its OpenSea Drops API
primary path (already scoped out of this project).

### 24a — risk validation checklist: partially real, one claim is dead code

Read `src/risk/` directly, not the README's summary of it:
- **`checkGoPlus` (`goplusCheck.ts`) is real and well-implemented**, and
  genuinely wired into the actual validation path
  (`validator.ts::validateMint` calls it). Hits GoPlus's real NFT security
  API (`nft_security/{chain_id}`), checks `is_honeypot` and
  `malicious_behavior`, fails open by default (a real, explicit design
  choice — `strictMode` flag controls whether an API failure blocks or
  just warns). Their own comment confirms GoPlus does not cover Robinhood
  Chain (chain id 4663) yet.
- **`checkContractAge` and `checkEtherscanVerified` (`etherscanCheck.ts`)
  are real, correctly-implemented functions — and dead code.** Grepped the
  whole repo: **zero call sites outside their own definition file.**
  `validator.ts::validateMint` calls ONLY the blacklist check and GoPlus —
  the README's claim of "pre-mint checks including contract age (>1hr),
  Etherscan verification status" describes functions that exist and work
  in isolation but are never actually invoked in the live validation path.
  This is exactly the "don't take the README at face value" case the task
  anticipated.

**Recommendation: worth a follow-up implementation step, with a
correction.** GoPlus honeypot/malicious-contract scanning is a genuine,
currently-missing gap in seasniper's own 8b/8c target-resolve flow — right
now namesquatting gets a warning label (8c) and nothing else is
automated. GoPlus's NFT security API is free/accessible (same endpoint
seadrop-noir-bot hits, no signup barrier evident from their code). Sketch,
not implemented: add a `resolve_address`-adjacent check in `target.rs`
that hits `https://api.gopluslabs.io/api/v1/nft_security/{chain_id}?
contract_addresses={addr}`, checked against `is_honeypot`/
`malicious_behavior`, fail-open by default (matching seadrop-noir-bot's
own justified choice — a security API being briefly unavailable should
not block an otherwise-legitimate target), surfaced as an additional
warning tier alongside the existing namesquatting warning, not a hard
block. The contract-age/Etherscan-verification idea is real and worth
copying too, but implement it for real (actually call it from the
resolve path) rather than reproducing seadrop-noir-bot's own gap of
building it and never wiring it in.

### 24b — NTP clock drift check: real, wired in, one accuracy correction

Read `src/utils/clockCheck.ts` directly. **Not actually NTP** — the
README's framing is a slight mischaracterization of the real mechanism:
it's an HTTP `Date`-header comparison against Cloudflare's
`/cdn-cgi/trace` endpoint, RTT-compensated (adds half the round-trip to
the local timestamp before comparing), with an explicit, documented
~1-second inherent quantization error from the `Date` header's own
resolution. Threshold 1500ms, chosen specifically to sit above that
quantization noise. Non-blocking: logs a warning, never refuses to start.
**Confirmed genuinely wired into their boot sequence** — grepped for the
call site: `src/index.ts` imports and awaits `checkClockDrift()`, not
just defined-and-unused the way 24a's Etherscan checks were.

**Recommendation: worth a small follow-up step.** This is real, cheap
(one HTTP HEAD request, no new dependency — `fetch` is already available),
and directly protects `timestamp`-mode's entire correctness model, which
depends on the VPS's system clock being accurate — currently nothing in
`main.rs`'s boot sequence checks this at all. Sketch: port an equivalent
check into `main()` before `config::Config::load` or right after, log via
both `bus::log` and `tracing::warn!` (this project's own established
"both, not either" standard from step 17's finding), and — the one
deliberate improvement over seadrop-noir-bot's own choice worth
considering at implementation time — decide whether `timestamp`-mode
triggers specifically should refuse to arm above some threshold, rather
than only warning, given how directly this bot's core trigger mode depends
on clock accuracy (their bot logs and continues either way; this project's
own "reject a bad shape early" convention, used throughout `config.rs`'s
`validate()`, argues for being stricter here, not just matching their
non-blocking choice by default).

### 24c — RPC benchmarking/ranking beyond broadcast-time racing: real infrastructure

Read `src/rpc/client.ts` directly. Real: `benchmarkAndRank()` probes every
configured custom endpoint's real `eth_blockNumber` latency, persists the
measured latency, and rebuilds a `viem` `fallback()` transport ordered
fastest-first (`rank: false` — they deliberately keep their own measured
order rather than letting viem's own dynamic ranking override it). The
exported `publicClient` is a `Proxy` that always forwards to the current
underlying (possibly-just-rebuilt) client, so every read call anywhere in
their codebase — not just a broadcast-time race — benefits from the
ranking once computed. This is genuinely broader than seasniper's own
`http_rpc_urls` racing, which only applies to the already-optimized
`fire_prepared` broadcast path (`warm_connections`/racing every configured
URL at fire time), not to `getPublicDrop` checks, balance polling, or any
other read.

**Recommendation: worth a follow-up step, tied directly to the still-open
15f/19 colocation question.** Step 15f's own finding was explicit:
`send_to_ack_ms`'s ~1.5x gap vs. MintDash is "plausibly explained by RPC/
network proximity" and is "the number a future colocation/dedicated-node
step could reasonably expect to move" — but colocation is real
infrastructure work (a new VPS location, at minimum). RPC ranking is a
cheaper lever aimed at the same underlying problem (this bot's own
distance/quality to whichever RPC it's actually talking to) without
needing new infrastructure — worth trying BEFORE colocation, not instead
of it necessarily, since it's strictly cheaper to attempt. Sketch, not
implemented: a periodic (or on-demand, admin-triggered) benchmark pass
over `http_rpc_urls`, persisting measured latency (a new small table or a
`config.rs` runtime field, not necessarily `identity.db` — implementation
detail), reordering read-path calls (NOT the already-optimized
`fire_prepared`/`warm_connections` hot path, which already races in
parallel and gains nothing from sequential ranking) to prefer the
fastest-measured endpoint first.

### 24d — per-chain profile config structure: real, clean reference shape

Read `src/chains.ts` directly. Real: a single `ChainProfile` interface per
`ChainKey`, centralizing `chainId`, RPC/explorer/OpenSea-slug metadata,
AND a nested `SnipeProfile` (priority fee, base-fee multiplier, hammer
interval, lead time, fixed gas limit) — one object per chain, not values
scattered across independent fields the way seasniper's own `block_time_ms`
etc. currently are. Their own Robinhood Chain profile comment
independently corroborates this project's own step 13 finding (no
priority-fee market, latency-only racing, `eth_maxPriorityFeePerGas`
returns 0) — real cross-validation from an independent codebase, not just
agreement with itself.

**Recommendation: worth a follow-up step, with InkChain (step 22, this
same session) landing as a live example why.** Right now,
`Config::looks_like_robinhood_chain()`'s existence in `config.rs` is
itself a symptom of the scattered-values problem this pattern would fix —
a per-chain-quirk special case bolted onto otherwise chain-agnostic
validation, because there's no single place "here's what's different
about this chain" lives. With three chains now confirmed relevant
(Ethereum, Robinhood Chain, InkChain) and step 22b independently finding
InkChain has its own quirks worth encoding (measured 1000ms block time,
unconfirmed FCFS, unconfirmed mempool visibility), the risk of a FOURTH
chain addition missing a needed per-chain tuning value (forgetting to set
`block_time_ms` correctly, the exact class of bug `looks_like_robinhood_
chain()`'s own validate() check exists to catch after the fact) grows with
every chain added under the current scattered-fields shape. Sketch, not
implemented: a `ChainProfile`-equivalent struct in `config.rs` (not a new
file — the task's own instruction) holding the fields that are genuinely
per-chain (block_time_ms, and — if steps 20/23's race_mode-adjacent work
lands — sequencer URL conventions, gas-jitter defaults), with `Config`
holding one `Option<ChainProfile>` or resolving one from `chain_id` at
validate() time, replacing ad hoc per-chain `if` branches like
`looks_like_robinhood_chain()` with a single lookup.

### OpenSea key expiry corroboration — confirmed, no action needed

seadrop-noir-bot's README states: "max 2 keys/day, expire after 7 days."
Grepped their README directly (not summarized): confirmed verbatim —
"Limits: read 600/h, write 30/h; **max 2 keys/day**, **expire after 7
days**." This matches RUNBOOK.md's step 21e entry (7-day instant-key
expiry) — independent corroboration from an unrelated codebase pointed at
the same real OpenSea API, not just internal consistency with this
project's own earlier finding. No action needed beyond this confirmation.

No code changes in this step.

## Step 26 — benchmark-token.sh redeploy grabbed the wrong address

Real bug found live: `redeploy` mode successfully deployed and configured
a fresh contract on-chain (two real `forge create` + `updatePublicDrop`
transaction pairs, both `status: 1`) but reported the wrong address as
"the deployed contract" both times, and neither address showed a live
public drop when checked. Root cause, confirmed directly against the two
real transactions: `forge create`'s stdout prints `Deployer: 0x...`
BEFORE `Deployed to: 0x...`, and the old parsing
(`grep -oE '0x[a-fA-F0-9]{40}' | head -1`) grabbed the first match —
the deployer's own EOA, not the contract. That misparsed address was then
reused as the target for BOTH `setMaxSupply` and `updatePublicDrop`, so
both calls silently no-op'd against the deployer's own EOA (an EOA has no
code to revert against) — the real contract was never actually
configured, not just misreported.

Fixed by extracting the parsing into `deploy/lib/parse_deployed_address.sh`
(strictly matches the `Deployed to:` line, hard-fails if it's absent
rather than falling back to a guess), with a regression test
(`deploy/tests/test-parse-deployed-address.sh`) built from tonight's real
addresses, confirmed to actually catch the original bug (reverting the
fix reproduces the exact live failure) before confirming the fix passes.
Wired into CI. **Explicit gap, not silently skipped:** the actual live
re-deploy re-verification (confirming the script's own printed address
matches what `getPublicDrop` independently reports as live, end-to-end
on the real chain) has not been performed — this session has no funded
Robinhood Chain testnet deployer wallet independent of the operator.

## Step 27 — run-benchmark.sh's silent failure between the liveness check and restore

Real bug found live: `run-benchmark.sh` failed with a bare non-zero exit
and zero diagnostic output between "confirming the benchmark token is
live" and its own config-restore step. Manually running the identical
`benchmark-token.sh check` command with the same RPC URL and the same
genuinely-live step 26 benchmark token
(`0x118fafd8511a04Df686e848425253c838B3a1a94`) succeeded cleanly — so
neither the token nor the check logic itself was broken; something in
how `run-benchmark.sh` invoked that check internally was.

**Two real, compounding bugs, both confirmed by reading the code
directly, not guessed:**

1. **Stale hardcoded address, no discovery mechanism.**
   `BENCHMARK_CHECK_ADDR` had exactly one hardcoded default — step 14b's
   *original* benchmark token (`0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9`)
   — with nothing that picked up a later `benchmark-token.sh redeploy`'s
   fresh address automatically, and the variable isn't even mentioned in
   `run-benchmark.sh`'s own "Usage:" example (only in the Prerequisites
   section below it). An operator following the documented usage
   literally, after step 26's redeploy replaced the token, would silently
   re-check the stale original address every run.
2. **The failure that address-check produced was then swallowed
   entirely.** `CHECK_OUTPUT=$(RPC_URL=... benchmark-token.sh check ...)`
   is a plain variable assignment under `set -euo pipefail` — bash
   propagates a failed command substitution's exit status to the
   assignment itself, so when the check failed (EXPIRED, given bug #1),
   `run-benchmark.sh` aborted immediately at that exact line, before
   `echo "$CHECK_OUTPUT"` ever ran. The real reason WAS captured into the
   variable — it just never reached the terminal. `cleanup()`'s own
   "exiting non-zero — see messages above" text was actively wrong in
   this specific failure mode, since there were no messages above to see.
   (Ruled out, with reasoning: an env-var-scoping/sudo-stripping issue for
   `TESTNET_HTTP_RPC_URL` — the script's own `${TESTNET_HTTP_RPC_URL:?...}`
   guard would have failed loudly at the top of the script, long before
   "confirming the benchmark token is live" ever prints, if that var were
   genuinely unset; it wasn't.)

**Fixed, both bugs, plus the general silent-failure problem:**

- **Discovery, not just documentation.** `benchmark-token.sh redeploy`
  now writes its fresh address to a gitignored state file
  (`deploy/.benchmark-token-address`) on success.
  `deploy/lib/resolve_benchmark_address.sh` resolves
  `BENCHMARK_CHECK_ADDR` with real precedence — an explicit env override,
  then that state file, then the original hardcoded fallback — so a
  redeploy's address is picked up automatically on the next
  `run-benchmark.sh` run with no manual copy-paste step to forget.
  `run-benchmark.sh` now also prints which address it resolved and from
  which source, before the check runs.
- **No more silent swallowing, at both ends.** `benchmark-token.sh
  check`'s human-readable diagnostics (STILL LIVE / EXPIRED / endTime
  unreadable) now go to stderr — they show up live, as they happen,
  regardless of what any caller does with stdout; only the
  machine-parseable `BENCHMARK_NFT_CONTRACT=` line stays on stdout.
  `run-benchmark.sh` no longer lets `set -e` silently abort past the
  check call either — it explicitly captures the real exit status and
  reports it plainly before deciding what to do next.
- Audited every other failure path in `run-benchmark.sh` against the
  "every failure prints the real reason" standard (`RUNBOOK.md`-grade,
  same bar as `benchmark-token.sh`'s cast-bracket fix and step 17's WS
  error-wrapping) — every other exit point already complied; this
  check-liveness step was the only real gap.

**Verified, precisely — what could be tested here, and what couldn't.**
`deploy/lib/resolve_benchmark_address.sh` is fully unit-testable without
a live chain (`deploy/tests/test-resolve-benchmark-address.sh`, 4
assertions: hardcoded fallback, state-file auto-discovery, env override
always wins, a garbage state file degrades safely) — confirmed it
actually catches the original bug by manually reproducing the old,
state-file-blind logic against the same fixture and showing it resolves
to the stale address. The silent-swallow fix was verified by simulating
both the old (stdout-diagnostics, no exit-status capture — reproduces a
completely silent, zero-output non-zero exit, exactly the live symptom)
and new (stderr diagnostics + explicit status capture — real reason
visible) invocation shapes side by side. Wired into CI's `deploy-scripts`
job. **What was NOT verified, stated plainly, same honesty standard as
step 26's closing note:** this session has no live VPS or funded testnet
wallet independent of the operator, so the actual end-to-end
`run-benchmark.sh` orchestration (state-file write on a real redeploy,
auto-discovery on the next real run, the full swap/fire/restore cycle)
has not been re-run live. Operator verification: run
`benchmark-token.sh redeploy`, confirm
`deploy/.benchmark-token-address` now holds the new address, then run
`run-benchmark.sh` with no `BENCHMARK_CHECK_ADDR` override and confirm
its own printed "using benchmark token address: ... (source: state-file)"
line names that same address.

cargo build/check/test: no Rust changes in either step 26 or step 27 —
both are `deploy/*.sh` fixes only.
