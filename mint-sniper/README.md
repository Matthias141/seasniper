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

1. **Compile and fix API drift.** This was written without a compiler in the
   loop (sandbox had no Rust toolchain). Alloy's API shifts across minor
   versions — expect a handful of signature mismatches on first `cargo build`.
2. **Dry-run on a testnet clone of the target contract** if you can get the
   bytecode/source (most ERC721A/manifold/thirdweb mints are open source or
   verified on Etherscan) — deploy identical mint-gating logic to Sepolia,
   fire the bot at it, confirm nonces/calldata/gas all land correctly.
   For seadrop mode, SeaDrop is also deployed on Goerli/Sepolia — verify
   the address on that chain's explorer before testing there, it isn't
   hardcoded to the mainnet address in `config.example.toml`.
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

Security note: the API has no auth and binds to loopback only on purpose.
Fine for "this runs on my own machine." If you ever run this on a VPS to
get better RPC latency, put it behind a tunnel (Tailscale, SSH port
forward) rather than exposing 4117 to the internet — there is nothing
stopping an unauthenticated caller from rewriting the mint target or
firing your wallets.

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
  watching nothing if the subscription isn't supported.
- Does not manage wallet funding/withdrawal logistics — that's a separate,
  equally important operational concern (gas top-ups, consolidating minted
  NFTs to a single wallet post-mint, etc.) left out of scope here.
