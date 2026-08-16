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

## Reaching this from your phone (step 10.5)

Two genuinely different paths exist, for two different situations. Pick
ONE — see 10.5a below for why running both at once is actively worse,
not just redundant.

### Path 1 — Tailscale (install the Tailscale app on your phone)

`google_oauth_redirect_url` in `config.toml` points at a Tailscale
MagicDNS HTTPS hostname (e.g. `https://sniper.your-tailnet.ts.net`).
Works on any device joined to your tailnet, including a phone with the
Tailscale app installed. Nothing is reachable off your tailnet — no
public DNS record, no public attack surface at all. This is the
original step 10c setup and needs no further steps here.

### Path 2 — Cloudflare Tunnel + Access (phone reachability, no app install)

For reaching the bot from a phone browser with nothing installed beyond
the browser itself. Two layers, stacked: Cloudflare Access gates the
tunnel hostname at Cloudflare's network edge (before any request reaches
this machine at all); step 10's own Google + TOTP + WebAuthn + step-up
auth is still the real authorization boundary for what a signed-in
request is actually allowed to do. **Access does not replace step 10's
auth — it's an outer perimeter in front of it, not a substitute for
it.** A leaked/misconfigured Access policy alone cannot arm or fire this
bot; it can only get someone as far as step 10's own login wall.

**10.5a — one canonical origin, not two.** WebAuthn passkeys are bound
to the origin they were registered under. Rather than maintain a
Tailscale-origin AND a separate Cloudflare-origin as two live contexts
(which would mean each physical device burns 2 of its own 2-credential
admin cap just registering against both — `trg_webauthn_admin_cap`
caps per user, not per origin, so a laptop registered on both origins
alone would exhaust the entire cap with zero room left for a phone),
this project uses **exactly one origin for everything**:
`google_oauth_redirect_url`'s host, which becomes both Google's OAuth
redirect target AND the Cloudflare Tunnel's public hostname AND
WebAuthn's rp_origin — see `identity::webauthn::derive_origin`'s doc
comment and `Config::google_oauth_redirect_url`'s doc comment in
`config.rs`. Practical implication: **there is no simultaneous
Tailscale-origin + Cloudflare-origin setup.** Switching from Path 1 to
Path 2 means changing `google_oauth_redirect_url` to the new public
hostname, updating Google Cloud Console's registered Redirect URI to
match, and — because WebAuthn credentials are origin-bound with no
migration path — re-registering every passkey under the new origin
(old ones simply stop validating; this is the same "revoke and
re-register" flow the 2-device cap already assumes, not a special
case). Once switched, the Cloudflare hostname works from every device,
including ones still on your tailnet — there is no reason to keep
Tailscale as an auth origin once Cloudflare Access is in front, since
the public hostname is reachable from anywhere Tailscale already
reached plus everywhere else.

**Setup:**

1. **cloudflared, pointed at the existing local bind — the bind address
   itself does not change.** The Rust binary still binds
   `127.0.0.1:4117` only (`API_BIND_ADDR` in `main.rs`, unchanged by this
   step); `cloudflared` runs alongside it as a local client that reaches
   IN to that address, not something the bot's own bind address widens
   to accommodate:
   ```bash
   cloudflared tunnel login                       # opens a browser, authenticates against your Cloudflare account
   cloudflared tunnel create mint-sniper
   cloudflared tunnel route dns mint-sniper sniper.your-domain.com
   cloudflared tunnel run --url http://127.0.0.1:4117 mint-sniper
   ```
   Confirm explicitly after starting it that `ss -tlnp | grep 4117` (or
   equivalent) still shows the Rust process listening on `127.0.0.1`,
   not `0.0.0.0` — cloudflared needs no inbound port opened on this
   machine at all, it makes an outbound connection to Cloudflare's edge.

2. **Update `config.toml`:**
   ```toml
   google_oauth_redirect_url = "https://sniper.your-domain.com/auth/google/callback"
   ```
   and update the matching Redirect URI in Google Cloud Console's OAuth
   client settings to the exact same URL — Google rejects any mismatch.
   CORS picks up the new origin automatically from this same value (see
   `api::router`'s doc comment) — there is no separate CORS config field
   to also update.

3. **Cloudflare Access**, configured in the Cloudflare dashboard (Zero
   Trust → Access → Applications) against the tunnel hostname:
   - Application type: Self-hosted, hostname `sniper.your-domain.com`.
   - Login method: **Google, reusing the same OAuth setup step 10c
     already configured** — one fewer credential to manage, and keeps
     "who is allowed to even reach the login page" scoped to the same
     Google account(s) as "who is allowed to actually sign in," rather
     than introducing email-OTP as a second, independent identity
     system with its own account list to keep in sync.
   - Policy: Allow, scoped to specific email(s) — at minimum your own
     account. Leave room for step 11's operators by making this an
     Access **group** (Zero Trust → My Team → Groups) rather than a
     flat list of emails hardcoded into the policy — step 11 can then
     add an operator to the group at invite time without touching the
     Access policy itself.
   - **What this layer does and does not do, stated precisely:** Access
     blocks an unauthenticated request at Cloudflare's edge — real
     attack-surface reduction against credential stuffing, scanning, and
     casual scraping, since none of that traffic ever reaches this
     machine's Rust process at all. It does NOT know or care about
     step 10's admin_tier/step-up state — someone who passes Access but
     has no `admin_tier` session still hits step 10's own login wall
     exactly as if they'd connected directly. Do not describe Access as
     making step 10's app-level auth redundant; it isn't, and step 10's
     auth remains fully active and fully required behind it.

4. **Verify the full chain live, from an actual phone browser** — not
   just that `cloudflared` is running:
   1. Open `https://sniper.your-domain.com` on the phone.
   2. Cloudflare Access's own login page appears FIRST — sign in with
      the Google account on the Access policy's allow-list.
   3. Only after that does step 10's own app load and prompt for Google
      Sign-In → TOTP → WebAuthn, exactly as it would over Tailscale.
   4. Confirm a device NOT on the Access policy is rejected at step 2
      and never reaches step 3 at all.
