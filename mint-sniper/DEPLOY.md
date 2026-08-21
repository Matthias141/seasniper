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
7. **Verify the storage step actually took — don't trust the wizard,
   check.** STEP 16 FINDING: this exact miss happened live on the
   first real deploy — the wizard defaulted back to 8GB despite step 5
   above, root filled to 100% partway through the build, and it needed
   a live resize to recover. Before doing anything else on the box:
   ```bash
   df -h /
   ```
   Confirm the root filesystem shows ~20GB, not ~8GB. **If it shows
   ~8GB, fix it now, before section 2** — resizing later after the
   disk is already full is more disruptive than catching it here:
   1. AWS Console → EC2 → Volumes → select the instance's root volume
      → Actions → **Modify volume** → set size to 20 → Modify. Takes
      effect within a few minutes; no reboot needed.
   2. Back on the instance, grow the partition and filesystem to fill
      the resized volume:
      ```bash
      sudo growpart /dev/nvme0n1 1   # device name may differ — check `lsblk` first
      sudo resize2fs /dev/nvme0n1p1  # match the partition growpart just grew
      ```
   3. Re-run `df -h /` and confirm it now shows ~20GB before continuing.
8. Point a DNS record at it if using step 10.5's Cloudflare Tunnel path
   (the tunnel needs no inbound port open — see `ui/README.md`'s
   Cloudflare section — but you'll still want SSH access directly for
   this deploy).

## 2. One-time OS setup (operator, on the VPS)

**STEP 16 FINDING — read before running the old (broken) sequence.**
The first real deploy installed Rust as the `ubuntu` login user
(the natural result of running rustup's one-liner as yourself), which
puts the toolchain in `~ubuntu/.cargo`. `ubuntu`'s home directory has
default `750` permissions (`drwxr-x---`) — no other user, including
`mint-sniper` (the account `deploy.sh` actually builds as), can even
traverse into it. This surfaces as `cargo: command not found` under
`sudo -u mint-sniper`, which reads exactly like Rust never installed —
it did, just somewhere `mint-sniper` structurally cannot reach. Do NOT
try to fix this by loosening `ubuntu`'s home permissions; install the
toolchain for the right user from the start instead:

```bash
# 1. Create the mint-sniper system user NOW, not later — deploy.sh
#    (section 3) creates it too if it doesn't exist yet, so running
#    this here is not a duplicate step, it's doing it early on purpose
#    so the Rust install below lands in the right home directory.
sudo useradd --system --create-home --home-dir /opt/mint-sniper --shell /usr/sbin/nologin mint-sniper || true

# 2. Rust toolchain — installed FOR mint-sniper, not for your own
#    login user. `-H` sets HOME to mint-sniper's home for this command,
#    so rustup lands the toolchain in /opt/mint-sniper/.cargo —
#    exactly where deploy.sh's later `sudo -u mint-sniper cargo build`
#    call will look for it, no cross-user permission problem possible.
sudo -u mint-sniper -H bash -c \
  "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"

# Node.js (system-wide install via apt — no per-user issue here, unlike
# Rust above; every user on the box can already see /usr/bin/npm)
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt-get install -y nodejs git build-essential pkg-config libssl-dev
```

**Swapfile — required on `t4g.small`, not optional.** STEP 16 FINDING:
even with the Rust-install fix above, `cargo build --release` was
killed outright by the kernel OOM killer (`signal: 9, SIGKILL`) on the
first real deploy. This codebase's release profile deliberately uses
LTO + `codegen-units=1` (not something to change just to dodge this —
it's a real, intentional tradeoff for a sniper bot's binary), and LTO's
link step spikes memory hard with no headroom left on `t4g.small`'s
2GB RAM. A 4GB swapfile fixed it immediately — the next build attempt
compiled clean in under 2 minutes. This is a harder requirement than
DEPLOY.md's earlier "2GB is less than the ~4GB original class, watch
memory usage" framing suggested — it's not a soft thing to monitor
after the fact, the release build does not complete without this:

```bash
sudo fallocate -l 4G /swapfile
sudo chmod 600 /swapfile
sudo mkswap /swapfile
sudo swapon /swapfile
# Persist across a reboot — without this line the swapfile is gone
# (and the OOM kill comes back) the next time the instance restarts.
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
free -h   # confirm ~4G shows under the Swap row before moving on
```

Tailscale (if using step 10.5's Tailscale-only path) or cloudflared (if
using the Cloudflare Tunnel + Access path — see `deploy/setup-cloudflared.sh`,
step 16b, and `ui/README.md`'s "Reaching this from your phone" section
for which one and why). Neither is required for the bot to run; both
are required for reaching it from your phone.

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

## 9. Step 15c-15e — the real PUSH benchmark, once this VPS is live

This is the actual point of getting a real VPS running — closing gap
#11 for real (this bot's WS transport has never once connected outside
a coding sandbox whose own TLS-interception proxy blocks it) and
getting a genuinely PUSH-based `dispatch_to_inclusion_ms` number
instead of step 14b's honestly-labeled-but-still-POLL-based one. All
three scripts below are prepared and ready to run; none of them have
been run by any coding session, since none of them can reach this VPS.

**15c — confirm or redeploy the benchmark token:**
```bash
export RPC_URL=https://robinhood-testnet.g.alchemy.com/v2/YOUR_KEY   # your own testnet key
./deploy/benchmark-token.sh check 0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9   # step 14b's original
```
If that reports EXPIRED (likely, by the time a real VPS exists — it
was deployed "live for 7 days" back in step 14b):
```bash
export DEPLOYER_PK=0x...   # a Robinhood Chain TESTNET-funded key — see the
                            # script's own header comment before running
./deploy/benchmark-token.sh redeploy
```
Note the new `nft_contract` address it prints — you'll need it for
`config.toml` below. Foundry (`forge`/`cast`) needs installing first if
it isn't already — the script's header comment covers this: this
session could never get Foundry working in its own coding sandbox
(GitHub release fetch 403'd due to this session's own scoped network
access), but that was a sandbox-specific limitation, not a Foundry one
— a real VPS with normal internet access should install it the
standard way (`curl -L https://foundry.paradigm.xyz | bash && foundryup`)
with no special workaround needed.

**Update `config.toml`** with the confirmed-live (or freshly redeployed)
token, then restart:
```toml
mint_mode = "seadrop"
nft_contract = "<address from above>"
fee_recipient = "0x0000a26b00c1F0DF003000390027140000fAa719"
quantity_per_wallet = 1
block_time_ms = 227   # step 14b's real measured Robinhood testnet figure
```
Keep exactly ONE `[[wallets]]` entry for this — the benchmark script's
methodology assumes sequential single-wallet fires, same as step 14b.
```bash
sudo systemctl restart mint-sniper
```

**15d — confirm PUSH actually engages (not silently falling back to
POLL, which is what every single benchmark before this one has been
stuck doing per gap #11):**
```bash
sudo journalctl -u mint-sniper -f
# in another terminal, arm once:
curl -sS -X POST -H "Authorization: Bearer $(cat /opt/mint-sniper/.sniper-token)" http://127.0.0.1:4117/api/arm
```
Look for the line `inclusion detection: WS push path established for
this arm session` in the journal output. If it instead says `WS push
path unavailable, using HTTP poll fallback`, gap #11 is NOT closed on
this box — check `ws_rpc_url` is reachable from here directly
(`curl` won't test WS, but a bad/unreachable URL, an expired key, or a
firewall egress rule blocking outbound 443 to Alchemy would all produce
this) before running the full benchmark. This log line was previously
only visible in the live browser UI at the exact moment of arming —
neither `journalctl` nor `audit.log` carried it (`bus::log`'s events
don't reach either — see CLAUDE.md's step 17 finding on this same
gap). Fixed alongside this step so it's checkable after the fact, same
as everything else on this VPS.

**15e — run the actual benchmark:**
```bash
cd /opt/mint-sniper
sudo -u mint-sniper /opt/mint-sniper/repo/mint-sniper/deploy/run-benchmark.sh 15
```
Read the script's own header comment for prerequisites BEFORE running
— it will not check for you whether your testnet ETH faucet transfer
actually landed, and it cannot automate a step-up TOTP loop if identity
(step 10c) is enabled on this instance. Prints a p50/p90 summary for
both `send_to_ack_ms` and `dispatch_to_inclusion_ms` at the end, plus a
push-vs-poll count cross-checking 15d across the whole run.

**15f — update CLAUDE.md** with the real numbers this produces,
following step 15e's own printed reminder at the end of its output.

## What this checklist does NOT cover

DNS/Cloudflare Access policy setup (see `ui/README.md` §10.5c),
incident response (see `RUNBOOK.md`), and anything past "the bot is
running and reachable" — those are separate, already-documented
concerns this file intentionally doesn't duplicate.
