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

## Security model (step 10g) — CORS allow-list, plus exactly ONE of two auth modes

Binding to `127.0.0.1` stops anything off-machine, but not everything on
it. With no auth and permissive CORS, any webpage open in the same
browser — a malicious tab, a bad ad, a compromised site — could
`fetch("http://127.0.0.1:4117/api/arm", { method: "POST" })` from its own
JS and the browser would let it through: DNS-rebinding and localhost-CSRF
are real, known attack classes against exactly this shape of
unauthenticated local API.

**CORS is an explicit allow-list** (`http://localhost:5173` dev,
`http://127.0.0.1:4117` / `http://localhost:4117` prod), not `Any`. No
wildcard fallback. This applies regardless of which auth mode below is
active.

**Every protected route requires exactly ONE of two mechanisms — never
both, decided once at boot by whether Google Sign-In is configured
(`google_oauth_client_id` set in `config.toml`).** This is a step 10g
decision, not an oversight: the two mechanisms are never left coexisting
with unclear precedence for the same request. See `src/auth.rs`'s module
doc comment for the full reasoning.

### Mode A — identity not configured: local bearer token (unchanged since step 7b)

Every route requires a bearer token (`Authorization: Bearer <token>` for
HTTP, `?token=` query param for the `/ws/events` WebSocket upgrade
specifically — browsers can't set custom headers on a `WebSocket`
handshake) except `GET /api/token`, which is how this UI bootstraps the
token at startup (`initAuth()` in `src/lib/api.ts`, called once before
anything else renders — see `App.tsx`). The token lives in
`.sniper-token` at the repo root, generated on first run, gitignored,
`chmod 600` on Unix.

**What this protects against:** a malicious webpage open in the same
browser trying to silently arm/fire/reconfigure the bot via `fetch` or a
raw `WebSocket` — it has no way to know or guess the token, and CORS
stops it from even reading a response that might leak one.

**What this does NOT protect against, precisely:** anything that already
has filesystem access to `.sniper-token` — native malware running as the
same OS user, a compromised browser extension with broad host/file
permissions, another process on a shared/multi-user machine that can read
your files. Reading the token file directly is just as good as stealing
it over HTTP. It also doesn't identify WHO is connecting — anyone with
the file is indistinguishable from the operator. That's a local-agent
auth model (stops arbitrary web content from reaching the API), not a
defense for a fully compromised machine, and not per-identity auth —
which is exactly why Mode B exists.

### Mode B — identity configured: per-identity session auth (step 10)

The bearer token is **not checked at all** in this mode — a valid,
`admin_tier` session (Google Sign-In → TOTP → a registered WebAuthn
device, all completed at login; see `CLAUDE.md`'s "Identity (step 10)"
section for the full model) is required instead, via an httponly,
Secure, encrypted session cookie. `GET /auth/session` lets this UI check
sign-in state; `/auth/google/login`, the TOTP setup/verify routes, and
the WebAuthn registration/authentication routes are how a session gets
established/progressed and are the only routes outside this gate besides
`GET /api/token`.

**Step-up auth on top of that (10f):** an `admin_tier` session is enough
to view wallet status and the event feed, but NOT enough by itself to
arm, fire, change config, or change the mint target — those additionally
require a fresh TOTP code on that specific request (`X-Step-Up-Totp`
header), checked live against the same replay-protected TOTP flow a
login uses. A session doesn't get a pass for being old; a code doesn't
get reused because it worked a minute ago for something else.

**What this protects against, beyond Mode A:** identifies WHO is
connecting, not just THAT some browser tab has a token file — the wrong
person (or a stolen/leaked `.sniper-token`, since it's not even checked
in this mode) cannot arm or fire without also passing Google + TOTP +
a registered physical device, and cannot do so on a stale, already-open
session without a fresh code on the specific request that matters.

**What this does NOT protect against, precisely:** the same
fully-compromised-machine caveat as Mode A applies at the far end — if
malware runs as the same OS user as the bot, it can read `identity.db`
and `.session-key` directly, same as it could read `.sniper-token` in
Mode A. Per-identity auth raises the bar for a REMOTE or credential-only
attacker; it does not create a security boundary against code already
running as you on this machine, and was never designed to. `.session-key`
is used for both the cookie signing/encryption key and TOTP-secret
encryption — see `identity/crypto.rs`'s doc comment for why one file, not
two.
