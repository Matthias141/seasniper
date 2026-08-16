# RUNBOOK

Concrete, actionable steps for the incidents this bot's own threat model
calls out — not prose, not "it depends." If you're here, something has
already gone wrong or you need to confirm something did or didn't. Follow
the numbered steps in order; each is written to be done, not pondered.

Cross-reference: `CLAUDE.md`'s "Known gaps" section for what's unverified,
`ui/README.md`'s "Security model" section for exactly what the API token
does and doesn't protect against.

---

## 1. Suspected private key compromise

Trigger: a wallet's `SNIPER_PK_*` env var may have leaked (committed by
mistake, shell history synced somewhere, shared machine, leaked via a
compromised dependency, etc).

1. **Stop the bot immediately** if it's running: `Ctrl-C` the `cargo run`
   process, or `POST /api/abort` first if you want a clean disarm logged
   before killing it. Either way, kill the process — don't leave a signer
   holding that key in memory.
2. **Identify every wallet using the suspected key.** Open `config.toml`,
   check the `private_key_env` value under `[[wallets]]` for each entry —
   confirm which `SNIPER_PK_N` the compromised key was assigned to, and
   which on-chain address that resolves to (the bot logs each wallet's
   address at boot — check the terminal scrollback, or run the bot once
   more locally with a throwaway config just to read the derived address
   if you no longer have it recorded).
3. **Move funds out of that wallet immediately**, to a wallet whose key
   was generated fresh and has never been used with this bot or any other
   tool. Use a hardware wallet or a freshly-generated key from a trusted
   offline source for the destination — not another `SNIPER_PK_*` slot,
   in case the compromise is broader than one key (shared clipboard
   history, shared shell history, etc).
   - If the wallet holds ETH only: a plain transfer covers it.
   - If it holds NFTs minted by this bot: transfer those too, in a
     separate tx from the ETH sweep isn't required, but confirm each NFT
     transfer individually — don't assume a bulk transfer tool got all of
     them.
   - If you're not sure what the wallet holds: check it on a block
     explorer (Etherscan/Basescan/etc, by chain) before assuming it's just
     ETH.
4. **Assume the leaked key is permanently burned.** Do not reuse it, do
   not "just rotate the env var and keep the address." Generate a
   brand-new key for that wallet slot going forward.
5. **Update `SNIPER_PK_N`** in your shell environment (or wherever it's
   sourced from — `.env`, secrets manager, etc) to the new key. Confirm
   `config.toml` itself was never holding the raw key — it only ever
   holds the env var *name* (`private_key_env`), by design (see
   `CLAUDE.md`'s architecture section) — so no edit is needed there beyond
   double-checking that's still true.
6. **Check how it leaked before restarting.** Run `git log --all -p -- '*config*'`
   and `git log --all -S 'SNIPER_PK'` from the repo root to rule out it
   ever having been committed. Check shell history (`history | grep -i
   sniper_pk` or your shell's equivalent) for the key having been typed in
   plaintext instead of sourced from a file. Fix the actual leak vector,
   not just the symptom, or the new key ends up compromised the same way.
7. **Restart the bot** only once the new key is in place and you've
   confirmed the old wallet's funds are moved.

---

## 2. Post-fire verification — did a mint actually succeed?

Trigger: the bot fired (auto or manual), and you need to know, with
certainty, what actually happened on-chain per wallet — not just what the
UI currently shows.

1. **Check the event feed first**, either in the PWA's event log panel or
   the raw WS stream. Look for `mint_result` events — one per wallet that
   fired. Each has `success: true/false` and a `detail` string.
   - `success: true` → the tx landed on-chain with status `1`
     (confirmed success, not just "broadcast"). This is the fix from
     `CLAUDE.md`'s gap #7 — the bot used to report broadcast as success
     even when the tx later reverted; it doesn't anymore. `detail` on a
     success still doesn't currently include the tx hash explicitly — see
     step 3 below for how to get it independently if you need one.
   - `success: false` → the tx landed on-chain but **reverted**. `detail`
     includes the tx hash, block number, and gas used (no revert-reason
     decoding — see gap #7's note on why that's out of scope). Copy the
     tx hash out of `detail` and go to step 3.
   - No `mint_result` event for a wallet at all → that wallet's tx never
     landed (RPC error before broadcast, connection failure, etc) — check
     for a preceding `log` event at `error` level from the same wallet,
     it'll have the underlying error.
2. **The event feed is ephemeral, not a log.** `bus.rs`'s broadcast
   channel holds the last 256 events and nothing more — if you reconnect
   the UI after a gap, or the bot's been running a while, the
   `mint_result` you need may already be gone from the buffer. If you
   don't see it: check the terminal where `cargo run` is running — logged
   events also print there via `tracing`. If that's gone too (terminal
   closed, output not captured), you have no durable record until gap 7g
   in `CLAUDE.md` (a persistent audit log) lands — this is a known,
   tracked gap, not something to assume works today.
3. **Cross-reference independently on a block explorer**, don't trust the
   bot's own report alone for anything involving real money — this is
   exactly how the nonce-drift and false-success bugs were originally
   caught (see `CLAUDE.md`'s "found live" gap notes).
   - If you have a tx hash (from `detail`, or from watching the wallet
     address directly): look it up on the relevant chain's explorer
     (Etherscan for mainnet, Sepolia Etherscan for testnet, Basescan for
     Base, etc). Confirm the status shows **Success**, not **Fail**, and
     that the `to` address matches the mint contract you targeted.
   - If you don't have a tx hash: open the explorer, search the wallet
     address directly, and look at its most recent outgoing transaction —
     the timestamp should line up with when the bot fired.
   - Confirm token ownership directly: call the NFT contract's
     `balanceOf(walletAddress)` (via the explorer's "Read Contract" tab,
     or `cast call <contract> "balanceOf(address)(uint256)" <wallet> --rpc-url <url>`
     if you have `cast` installed) and confirm it increased by the
     expected quantity.
4. **If a wallet reports `success: false` (reverted) and you don't know
   why**, common causes to check first: wallet already at
   `maxTotalMintableByWallet` cap (seadrop mode) or an equivalent
   per-wallet cap in a custom contract, mint not actually live yet at
   fire time (trigger fired early), insufficient ETH for `mint_value` +
   gas, or the mint sold out between prepare and fire (block-race — not a
   bug, just lost the race).

---

## 3. Wallet funds unexpectedly moved / drained

Trigger: a wallet's balance dropped and you didn't authorize it (not from
a mint you fired, not a gas cost you recognize).

1. **Do not panic-transfer other wallets' funds through this bot.** If the
   private key material itself might be compromised, the bot's own signer
   process is not a trustworthy tool to move money right now — see step
   6 below first.
2. **Confirm it's actually unauthorized** before treating it as an
   incident: check the tx on a block explorer. Does the `to` address match
   a mint contract you configured, or something you don't recognize? Does
   the timestamp line up with a time the bot was armed and firing, or with
   a time it definitely wasn't running?
3. **If confirmed unauthorized**: treat this identically to
   [section 1, "Suspected private key compromise"](#1-suspected-private-key-compromise)
   for that specific wallet — the fact that funds already moved means the
   key is compromised, full stop, regardless of how. Follow steps 1-6
   there for every wallet sharing that key or generated the same way
   (same seed phrase, same wallet-generation script run in the same
   compromised environment, etc) — not just the one wallet you noticed
   first.
4. **Check every other configured wallet immediately**, not just the one
   that alerted you — a single compromised environment (this machine,
   this shell, this git history) plausibly exposed all of them at once,
   not just one.
5. **Preserve evidence before you do anything else destructive**: note
   the draining tx hash(es), timestamps, and destination address(es).
   You'll want these if pursuing recovery, reporting to an exchange the
   funds moved through, or just understanding the blast radius later —
   don't let cleanup activity (moving remaining funds, restarting the
   bot) happen before you've written this down somewhere durable.
6. **Do not restart the bot against any wallet on this list until its key
   has been rotated per section 1.** Restarting with a compromised key
   still loaded just hands the attacker another round.

---

## 4. API token compromised

Trigger: `.sniper-token` may have leaked (committed by mistake, read by a
compromised browser extension, exposed via a misconfigured reverse proxy,
shared machine, etc). See `ui/README.md`'s security section for exactly
what this token does and doesn't protect — this section assumes that
threat model.

1. **Stop the bot** (`Ctrl-C` the `cargo run` process, or `POST /api/abort`
   first for a clean disarm if you have time). This is not itself a fix —
   the token file on disk is what matters — but it stops anything
   currently using the old token from firing while you rotate it.
2. **Delete the token file**: `rm .sniper-token` from the repo root (or
   wherever `TOKEN_PATH` in `main.rs` points, if that's ever been changed
   from the default).
3. **Restart the bot.** `auth::load_or_create_token` generates a fresh
   32-byte random token and writes a new `.sniper-token` on startup, since
   the old one is gone — confirm the file's modification timestamp is
   current, not stale, as a sanity check.
4. **Reload the UI** (hard refresh, not just re-navigate — clear any
   cached state) so it re-runs `initAuth()` and fetches the new token from
   `GET /api/token` instead of using a stale one it may have cached in
   memory or `localStorage` if that's ever added later.
5. **Confirm the old token no longer works**: with the bot running, try a
   request using the old token value (if you still have it) —
   `curl -H "Authorization: Bearer <old-token>" http://127.0.0.1:4117/api/status`
   — and confirm it now returns `401`.
6. **If the leak vector was git**, treat the token the same way you'd
   treat any committed secret: rotating it (steps 2-3 above) makes the
   *current* token safe, but the leaked one is still sitting in git
   history until it's rewritten out — check whether `.sniper-token` was
   ever actually committed (`git log --all -- .sniper-token`; it should be
   empty, since it's gitignored from the start — see `.gitignore`), and if
   it somehow was, that's a full history-rewrite situation, same severity
   class as `CLAUDE.md`'s own reason for existing (see the sibling
   AbiaEats project's `CLAUDE.md` for what that entails) — stop and
   escalate rather than improvising a fix under pressure.
7. **If the leak vector was a browser/extension**, the token rotation in
   steps 2-4 is the fix — reinstall or remove the compromised extension
   before trusting that browser with the new token.

---

## 5. Lost a WebAuthn device (step 10e)

Trigger: a laptop or phone holding one of your two registered passkeys
is lost, stolen, wiped, or its authenticator storage is reset.

**The normal case — you still have your OTHER registered device.** This
is fully self-service through the UI, no DB access needed:

1. Sign in on the surviving device (Google → TOTP → WebAuthn assertion
   from that device).
2. Open the device list (`GET /auth/webauthn/devices`) and identify the
   lost device's row.
3. Revoke it (`POST /auth/webauthn/devices/<id>/revoke`). This deletes
   the credential row outright — there is no "undo," the same as any
   other credential revocation.
4. If you want a replacement device registered, do that now — revoking
   first frees a slot under the 2-device cap (`start_registration`
   rejects a 3rd registration attempt with a clear error rather than
   silently overwriting or silently allowing it; see `identity/
   webauthn.rs`'s `ADMIN_CREDENTIAL_CAP`).

**The hard case — you lost BOTH registered devices at once (or the 2nd
was already lost and never revoked).** There is currently no self-service
path for this: per step 10f's model, even VIEWING wallet status requires
a session that has completed Google + TOTP + a WebAuthn assertion from a
still-registered device — with zero valid devices left, no session can
ever reach that state, and there is no "email yourself a recovery link"
flow (no SMTP is configured anywhere in this project, and step 10 is
explicitly scoped to one operator, not a multi-user system with an admin
who could intervene on your behalf).

The actual recovery path is direct access to the machine running the
bot — which you have, since this is a self-hosted single-operator tool,
not a SaaS you're locked out of. You are clearing your OWN device
registrations because you still control the server; this is not an
identity-verification problem the way a "forgot my Google password" flow
is.

1. **Stop the bot** (`Ctrl-C` the `cargo run` process). `identity.db` is
   SQLite in WAL mode; editing it while the process holds it open risks
   a corrupt read on the next boot.
2. **Open `identity.db` with any SQLite client** (`sqlite3 identity.db`
   if you have the CLI installed; this project's own binary vendors
   SQLite internally via `sqlite-bundled` but does not expose a query
   shell of its own, so bring your own — DB Browser for SQLite works too
   if you'd rather not use a terminal).
3. **Clear your WebAuthn credentials** so the 2-device cap no longer
   blocks re-registration:
   ```sql
   DELETE FROM webauthn_credentials WHERE user_id = (SELECT id FROM users WHERE email = 'you@example.com');
   ```
4. **If you ALSO lost access to your TOTP app** (not just the WebAuthn
   devices — these are separate factors and losing one doesn't imply
   losing the other), clear that too so 10d's setup flow can be re-run
   from scratch instead of rejecting a "wrong" code forever:
   ```sql
   DELETE FROM totp_secrets WHERE user_id = (SELECT id FROM users WHERE email = 'you@example.com');
   ```
5. **Restart the bot**, sign in with Google (still works — that identity
   was never touched), then redo whichever of 10d (TOTP) / 10e (WebAuthn)
   setup you cleared above.
6. **Treat this as a real incident, not routine maintenance.** Anyone
   who could run steps 1-4 above already had the level of access needed
   to fully compromise this bot regardless of step 10's identity layer
   (they're on the machine holding the wallet private keys) — but if
   this recovery was needed because a device was STOLEN rather than
   merely lost/wiped by you, also work through Section 1 (suspected
   private key compromise) and Section 4 (API token compromised) above,
   since a stolen laptop/phone may carry more than just a passkey.

---

## 6. Cloudflare Access policy compromised (step 10.5c)

Trigger: the Access application's allow-list was misconfigured (too
broad an email/group), a team member's Google account on the allow-list
was itself compromised, or you simply need to revoke public reachability
entirely — e.g. before extended travel, or the moment you notice
unexpected activity in Cloudflare's Access audit log.

**Read this first — what Access actually gates.** Per `ui/README.md`'s
10.5c section: Access blocks unauthenticated traffic at Cloudflare's
edge, before it ever reaches this machine. It is NOT step 10's
authorization boundary — someone who passes a compromised/over-broad
Access policy still hits Google Sign-In + TOTP + WebAuthn + step-up auth
exactly as before. **This means a compromised Access policy is a
reduced-attack-surface incident, not an "attacker can arm/fire" incident
by itself** — check step 10's own audit trail (below) before assuming
the worst, but still treat it seriously: it's the layer that was
supposed to keep credential-stuffing/scanning traffic away from this
process at all, and an attacker who gets THROUGH Access still gets to
try their luck against step 10's real auth, which they otherwise
wouldn't have been able to reach.

1. **Immediately tighten or disable the Access application** — Cloudflare
   dashboard → Zero Trust → Access → Applications → the tunnel's
   application → either edit the policy down to just your own account,
   or toggle the application off entirely. This takes effect immediately
   at Cloudflare's edge; no restart of this bot is needed.
2. **Check Cloudflare's Access audit log** (Zero Trust → Logs → Access)
   for who actually authenticated through the compromised policy and
   when — this tells you whether anyone besides you got past Access at
   all, which scopes how seriously to treat the rest of this list.
3. **Cross-reference against step 10's own record of what happened past
   Access**, since Access's log only proves someone reached the login
   wall, not what they did after: check `audit.log` (step 7g,
   DB-attribution pending step 11e) for any arm/fire/config-change/
   target-set event in the same window Access's log flagged, and check
   `identity.db`'s `sessions` table for any session created in that
   window you don't recognize
   (`SELECT * FROM sessions WHERE created_at > <window_start>;`).
4. **If step 3 shows a session or action you don't recognize**, this
   escalates to a real step 10 identity incident, not just an Access
   misconfiguration — work through Section 5 above (clearing WebAuthn/
   TOTP) for the affected user, and Section 1/4 if wallet-adjacent
   actions are involved.
5. **If step 3 comes back clean** (Access was reachable by more people
   than intended, but nobody who reached it got past step 10's own
   login), fixing the Access policy in step 1 is the complete remediation
   — no identity.db changes needed, since step 10's own boundary held.
6. **Rotate the underlying credential if the compromise was a Google
   account on the allow-list being taken over** (not just a policy
   configured too broadly) — remove that account from the Access
   group/policy AND, if that account also has a step 10 `users` row,
   treat it as Section 5's "lost device" hard case for that user, since
   whoever controls their Google account can now also pass step 10c's
   own Google Sign-In step.
7. **Once step 11 lands**, re-scope the Access group down to exactly the
   operators who still hold an active invite — an operator whose access
   was revoked at the step 10/11 identity layer but left in the
   Cloudflare Access group can still reach the login wall (harmless on
   its own, per this section's opening note, but pointless exposure);
   keep the two lists in sync as part of any operator offboarding.
