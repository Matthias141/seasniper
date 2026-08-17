# DEPLOY.md — first VPS deploy (step 15)

Everything through step 14 ran either in a coding sandbox or against
testnets from it. This is the checklist for the first time this bot
runs anywhere else — a real VPS, reachable (behind auth) from a real
phone, firing real money. Read `CLAUDE.md`'s "Live infra validation +
colocation decision (step 15)" section for the reasoning behind the
provider/region recommendation below; this file is the checklist, that
section is the "why."

**Provisioning the VPS itself and the first login to it are steps this
session cannot do autonomously** — no VPS account, no credentials, same
reason the Cloudflare API token and the `cloudflared` install had to be
done by the operator directly in step 10.5. Everything below this point
is written so that handoff is copy-paste, not guesswork.

## 0. Recommendation (see CLAUDE.md step 15 for the full research)

**Hetzner Cloud, Ashburn (US East, "ash") region, CPX11 or equivalent
small shared-vCPU instance** (2 vCPU / ~4GB RAM class — this workload is
SQLite + a few concurrent wallets + WS connections, not compute-heavy;
do not over-provision). Ashburn, Virginia is the same metro area as AWS
`us-east-1`, which real evidence points to for both sides of the
latency-sensitive path: Alchemy's own status history names "US East" as
a real serving region for their chain infra, and Robinhood's own
production stack visibly depends on AWS `us-east-1` (per its exposure
to the October 2025 `us-east-1` outage) — consistent with Robinhood
Chain's sequencer, which Robinhood operates directly, most likely
living in or very near the same region as the rest of Robinhood's AWS
footprint. Ubuntu 22.04/24.04 LTS image — plain Linux, no provider-specific
quirks expected for systemd/Tailscale/cloudflared.

## 1. Provision (operator does this)

1. Create the Hetzner Cloud server: Ashburn location, CPX11 (or
   nearest current equivalent — Hetzner's exact SKU names/pricing shift;
   pick the smallest shared-vCPU tier with ~4GB RAM), Ubuntu 24.04 LTS.
2. Note the server's public IP and confirm SSH access.
3. Point a DNS record at it if using step 10.5's Cloudflare Tunnel path
   (the tunnel needs no inbound port open — see `ui/README.md`'s
   Cloudflare section — but you'll still want to `ssh` in directly for
   this deploy).

## 2. One-time OS setup (operator, on the VPS)

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Node.js (for building ui/dist)
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt-get install -y nodejs git build-essential pkg-config libssl-dev

# Tailscale (if using step 10.5's Tailscale-only path) or cloudflared
# (if using the Cloudflare Tunnel + Access path) — see ui/README.md's
# "Reaching this from your phone" section for which one and why, and
# the exact cloudflared commands. Neither is required for the bot to
# run; both are required for reaching it from your phone.
```

## 3. Deploy the code

```bash
git clone https://github.com/Matthias141/seasniper.git /tmp/mint-sniper-deploy
sudo /tmp/mint-sniper-deploy/mint-sniper/deploy/deploy.sh source
```

(Or `deploy.sh release` once a `v*` tag has actually been pushed and
step 7d's release workflow has produced a tarball — check
`https://github.com/Matthias141/seasniper/releases` first; as of step
15, no tag has been cut yet, so `source` is the only real option.)

This creates a dedicated, unprivileged `mint-sniper` system user, builds
the release binary + `ui/dist`, and installs (but does not start) the
systemd unit. See `deploy/deploy.sh` and `deploy/mint-sniper.service`
for exactly what it does — nothing in it touches secrets or config.

## 4. Configuration (operator — this is where real values go)

```bash
sudo -u mint-sniper cp /opt/mint-sniper/repo/mint-sniper/config.example.toml /opt/mint-sniper/config.toml
sudo -u mint-sniper nano /opt/mint-sniper/config.toml
```

Fill in real RPC URLs (Alchemy — the whole point of the region choice
above is a short path to these), the target contract/SeaDrop details,
`google_oauth_*` fields if using step 10 identity (recommended for
anything reachable off `127.0.0.1`), and `block_time_ms` for whichever
chain you're actually targeting (12000 for mainnet/Sepolia, ~227 for
Robinhood Chain testnet per step 14b's real measurement — re-measure
for mainnet before relying on it, testnet and mainnet block times are
not guaranteed identical).

```bash
sudo cp /opt/mint-sniper/repo/mint-sniper/deploy/mint-sniper.env.example /opt/mint-sniper/mint-sniper.env
sudo nano /opt/mint-sniper/mint-sniper.env       # real private keys go here, NOT in config.toml
sudo chown mint-sniper:mint-sniper /opt/mint-sniper/mint-sniper.env
sudo chmod 600 /opt/mint-sniper/mint-sniper.env
```

## 5. Start it

```bash
sudo systemctl enable --now mint-sniper
sudo systemctl status mint-sniper
sudo journalctl -u mint-sniper -f     # tracing output — bus::log events are NOT here, see CLAUDE.md's note on that
```

Confirm `GET /api/token` responds on `127.0.0.1:4117` from the VPS
itself (`curl http://127.0.0.1:4117/api/token`) before setting up
Tailscale/Cloudflare — the bind address is still `127.0.0.1` only (see
`ui/README.md`'s security model; this has not changed by moving to a
VPS), so nothing off-box can reach it until the tunnel is configured.

## 6. Fund the wallets

Send real ETH (or the target chain's gas token) to each configured
wallet address. This is manual and deliberate — no automation touches
wallet funding, on purpose (see README.md's "What this does NOT do").

## 7. Confirm reachability from your phone

Follow `ui/README.md`'s "Reaching this from your phone" section
end to end (Tailscale app, or the Cloudflare Tunnel + Access flow) —
this is the first time that section's instructions get to run against
a real deployment instead of being read as a plan.

## 8. Updating later

```bash
sudo /opt/mint-sniper/repo/mint-sniper/deploy/deploy.sh source
sudo systemctl restart mint-sniper
```

`config.toml`, `mint-sniper.env`, `identity.db`, `.sniper-token`, and
`audit.log` all live directly in `/opt/mint-sniper/` (the systemd
unit's `WorkingDirectory`), untouched by a redeploy.

## What this checklist does NOT cover

DNS/Cloudflare Access policy setup (see `ui/README.md` §10.5c),
incident response (see `RUNBOOK.md`), and anything past "the bot is
running and reachable" — those are separate, already-documented
concerns this file intentionally doesn't duplicate.
