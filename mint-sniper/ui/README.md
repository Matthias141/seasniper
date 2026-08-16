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
- RPC health (`rpc_health` events) is real — `rpc_health_poll_loop` in
  `main.rs` pings every configured HTTP RPC every 15s and emits it.

## Security model — local bearer token + CORS allow-list

Binding to `127.0.0.1` stops anything off-machine, but not everything on
it. With no auth and permissive CORS, any webpage open in the same
browser — a malicious tab, a bad ad, a compromised site — could
`fetch("http://127.0.0.1:4117/api/arm", { method: "POST" })` from its own
JS and the browser would let it through: DNS-rebinding and localhost-CSRF
are real, known attack classes against exactly this shape of
unauthenticated local API. Two things fix that now:

1. **CORS is an explicit allow-list** (`http://localhost:5173` dev,
   `http://127.0.0.1:4117` / `http://localhost:4117` prod), not `Any`. No
   wildcard fallback.
2. **Every route requires a local bearer token** (`Authorization: Bearer
   <token>` for HTTP, `?token=` query param for the `/ws/events` WebSocket
   upgrade specifically — browsers can't set custom headers on a
   `WebSocket` handshake, so a header isn't an option there) except
   `GET /api/token`, which is how this UI bootstraps the token at startup
   (`initAuth()` in `src/lib/api.ts`, called once before anything else
   renders — see `App.tsx`). The token itself lives in `.sniper-token` at
   the repo root, generated on first run by `auth::load_or_create_token`,
   gitignored, `chmod 600` on Unix.

**What this protects against:** a malicious webpage open in the same
browser trying to silently arm/fire/reconfigure the bot via `fetch` or a
raw `WebSocket` — it has no way to know or guess the token, and CORS
stops it from even reading a response that might leak one.

**What this does NOT protect against, precisely:** anything that already
has filesystem access to `.sniper-token` — native malware running as the
same OS user, a compromised browser extension with broad host/file
permissions, another process on a shared/multi-user machine that can read
your files. Reading the token file directly is just as good as stealing
it over HTTP, and this auth model does nothing about that; it's a
local-agent auth model (stops arbitrary web content from reaching the
API), not a defense for a fully compromised machine. `GET /api/token`
itself is intentionally unauthenticated (it has to be, to bootstrap) but
is still behind the same CORS allow-list, so a disallowed origin can
trigger the request but the browser won't let its JS read the response.
