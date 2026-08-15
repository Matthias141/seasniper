# mint-sniper UI

PWA control deck for the Rust bot. Talks to it over the `/api/*` HTTP
endpoints and `/ws/events` WebSocket exposed by `src/api.rs`.

## Dev

```bash
# terminal 1 — the bot + API, from the repo root
cargo run

# terminal 2 — this UI, with hot reload
cd ui
npm install
npm run dev
```

Vite proxies `/api` and `/ws` to `127.0.0.1:4117` (see `vite.config.ts`), so
open `http://localhost:5173` and it talks to the real backend with no CORS
setup needed.

## Prod

```bash
cd ui && npm run build
```

Outputs to `ui/dist/`. Point `tower_http::services::ServeDir` at that
directory from the Rust binary (add a fallback route in `api.rs`'s router)
so one process serves both the control API and the installable PWA — no
second server, one less network hop between you and the fire button.

Install as a PWA from the browser's "Install app" prompt (or Add to Home
Screen on mobile) once served over `https://` or `localhost` — service
workers require a secure context.

## What's real vs placeholder here

- `public/icons/*.png` are generated placeholders (black background, amber
  triangle matching the in-app brand mark) — swap for real app icons before
  shipping if this leaves your own machine.
- RPC health (`rpc_health` events) is defined in the type contract and
  rendered in the log feed, but nothing on the Rust side currently pings
  RPCs and emits it — the connection-health pill in `StatusBar` currently
  only reflects the UI's own WebSocket connection, not per-RPC latency.
  Wire an actual health-check loop in Rust (parallel to `balance_poll_loop`)
  if you want that signal to mean something.
- Config edits write straight to `config.toml` on disk via `PUT /api/config`
  — no auth on the API. This binds to `127.0.0.1` only by design; do not
  change that bind address to `0.0.0.0` without adding authentication first,
  since anyone reaching port 4117 could rewrite your mint target or fire
  your wallets.
