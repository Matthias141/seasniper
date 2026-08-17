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

**AWS EC2, `us-east-1` region directly, `t4g.small` (2 vCPU / 2GB RAM,
Graviton/ARM, ~$0.0168/hr ≈ $12.26/mo).** Updated from an earlier
Hetzner/Ashburn recommendation, which is confirmed unavailable for this
operator — not a downgrade from that reasoning, actually a tighter
match to it: `us-east-1` is the literal region the evidence below
points at; Ashburn was only ever a same-metro approximation of
proximity to it, never the region itself. The reasoning is unchanged,
carried forward rather than dropped:
- Alchemy — a named Robinhood Chain infra partner — has its own status
  history naming "US East" as a real serving region for chain traffic
  (a July 2026 Hyperliquid latency incident was explicitly scoped to
  "US East region" in Alchemy's own status reporting), not just a
  generic multi-region claim.
- Robinhood's own production stack visibly depends on AWS `us-east-1`
  — it was among the services affected by the October 2025 `us-east-1`
  outage. Robinhood operates Robinhood Chain's sequencer directly (a
  single Arbitrum-Orbit sequencer, confirmed via Robinhood's own docs);
  the most likely place for it to live is the same region as the rest
  of Robinhood's AWS footprint — inference, not a confirmed fact from
  Robinhood's own docs, stated as such.

**Instance sizing, checked against current specs/pricing, not assumed
still right:** `t3.small` and `t4g.small` both give 2 vCPU / 2GB RAM in
`us-east-1` as of this writing. `t4g.small` (~$0.0168/hr) is ~19%
cheaper than `t3.small` (~$0.0208/hr) for the identical spec, because
it's Graviton (ARM64) rather than x86_64 — worth taking specifically
because `deploy.sh`'s `source` mode (the only real deploy path today,
see below) runs `cargo build --release` directly ON the target machine,
which compiles natively for whatever CPU architecture it's running on
with zero script changes — confirmed by reading `deploy.sh`, not
assumed. **Caveat, checked directly against `release.yml` (step 7d):**
that workflow's `runs-on: ubuntu-latest` runner only ever produces an
x86_64 Linux binary — no `aarch64` target exists in it. This means
`deploy.sh release` mode (downloading a prebuilt tarball) will NOT work
on a `t4g` instance until `release.yml` adds an ARM target; irrelevant
today (no `v*` tag has been cut yet, so `source` mode is the only real
option regardless — see §3 below) but worth knowing before relying on
`release` mode later. If you'd rather not deal with that distinction at
all, `t3.small` is the direct x86_64-compatible fallback at a small
premium.

**2GB RAM is less than the original Hetzner recommendation's ~4GB
class** — flagged explicitly, not silently substituted. This workload
(SQLite, a handful of concurrent wallet signers, a few WS connections)
has no obvious reason to need more, but watch actual memory usage after
the first deploy rather than assuming 2GB is definitely enough.

**Real cost this checklist didn't need under Hetzner:** EC2 bills
compute and storage separately. Add a `gp3` EBS root volume — 20GB is
comfortable headroom for the OS, Rust toolchain, build artifacts, and
this app's own files (`identity.db`, `audit.log`, etc.), at roughly
$1.60/mo on top of the instance price.

Ubuntu 22.04/24.04 LTS AMI — plain Linux, no provider-specific quirks
expected for systemd/Tailscale/cloudflared once the instance is up;
everything different about AWS specifically is in the networking setup
below, not the application layer.

## 1. Provision (operator does this)

AWS's launch flow differs enough from a plain VPS provider's that a
few of these steps have no Hetzner equivalent — read through once
before starting, don't assume it's the same flow with different button
labels.

1. **Confirm the region selector is explicitly set to `us-east-1` (US
   East, N. Virginia)** in the top-right of the AWS Console before
   doing anything else — AWS remembers your last-used region per
   browser/account, which may not default to `us-east-1`. This is the
   entire point of the recommendation above; launching in the wrong
   region silently defeats it with no error.
2. **Launch Instance** → AMI: Ubuntu Server 24.04 LTS (or 22.04) →
   Instance type: `t4g.small` (search "t4g.small" — note it's listed
   under the ARM/Graviton family, a different list than `t3.small`).
3. **Key pair** — AWS's SSH auth mechanism, not Hetzner's SSH-key-upload
   flow: create a new key pair (RSA or ED25519), download the `.pem`
   file immediately (AWS will not let you download it again), and
   `chmod 400` it locally. This is what you SSH in with — there's no
   password login by default.
4. **Network settings — Security Group.** Unlike Hetzner's default
   (SSH-reachable out of the box), AWS's default posture is deny-all
   inbound. The launch wizard offers to create a new security group —
   accept that, and set its one inbound rule: SSH (port 22), source
   restricted to **My IP** (not `0.0.0.0/0` — don't leave SSH open to
   the entire internet). **No other inbound port is needed**: the bot
   binds `127.0.0.1` only (unchanged by moving to a VPS — see step 0's
   note and `ui/README.md`'s security model), and step 10.5's
   Cloudflare Tunnel path needs no inbound port open at all. Launching
   via the standard wizard also auto-creates/uses your account's
   default VPC for this region — fine for a single always-on instance;
   there's no need to hand-roll a custom VPC for this.
5. **Storage** — bump the root volume to 20GB `gp3` (default is often
   8GB, too tight once the Rust toolchain and build artifacts are in
   place) before launching.
6. Launch, then note the instance's public IPv4 address (Console →
   Instances → your instance → "Public IPv4 address") and confirm SSH
   access: `ssh -i your-key.pem ubuntu@<public-ip>`.
7. Point a DNS record at it if using step 10.5's Cloudflare Tunnel path
   (the tunnel needs no inbound port open — see `ui/README.md`'s
   Cloudflare section — but you'll still want SSH access directly for
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
