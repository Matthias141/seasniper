#!/usr/bin/env bash
# STEP 16b — Cloudflare Tunnel setup for step 10.5's Cloudflare Tunnel +
# Access path. Same pattern as deploy.sh: the OPERATOR runs this on the
# real VPS, this session never does — no Cloudflare account access or
# credentials exist here, same reasoning as the EC2 .pem key and the
# Cloudflare API token before it.
#
# Read before running: whatever public hostname you route this tunnel
# to becomes THE canonical origin for this whole instance (Google OAuth
# redirect, WebAuthn rp_origin, CORS) per step 10.5a's explicit decision
# — see CLAUDE.md's "Cloudflare Tunnel + Access (step 10.5)" section for
# the full reasoning. If you already have WebAuthn passkeys registered
# under a DIFFERENT origin (e.g. a Tailscale MagicDNS hostname from an
# earlier setup), they stop validating the moment you switch
# google_oauth_redirect_url to this new hostname — that's expected
# (WebAuthn credentials are origin-bound, no migration path), not a bug
# to work around. Plan on re-registering devices right after, not
# mid-incident.
#
# What this script does NOT do, on purpose: create the Cloudflare
# Access application or its login policy (10.5c — a manual Cloudflare
# dashboard step, done directly by the operator with their own account
# access, same as this script itself), or touch the Cloudflare API
# token in any way. It only installs cloudflared and gets the tunnel
# itself running, pointed at this bot's actual bind address.
#
# Usage:
#   sudo ./setup-cloudflared.sh install     # installs cloudflared via Cloudflare's apt repo
#   ./setup-cloudflared.sh login-and-create # interactive — see step 2 below
#   sudo ./setup-cloudflared.sh service     # installs cloudflared.service (after step 2 has produced credentials)

set -euo pipefail

MODE="${1:-}"
TUNNEL_NAME="mint-sniper"
# The bot's real bind address — confirmed directly against
# src/main.rs's API_BIND_ADDR const before writing this script, not
# assumed unchanged since step 7b.
BOT_ADDR="http://127.0.0.1:4117"

case "$MODE" in
  install)
    if [[ $EUID -ne 0 ]]; then
      echo "run as root for 'install' (sudo ./setup-cloudflared.sh install)" >&2
      exit 1
    fi
    echo "==> adding Cloudflare's apt repository (architecture-agnostic — apt"
    echo "    resolves arm64 vs amd64 itself from this repo, nothing to pick manually)"
    mkdir -p --mode=0755 /usr/share/keyrings
    curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg | tee /usr/share/keyrings/cloudflare-main.gpg >/dev/null
    echo 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main' \
      | tee /etc/apt/sources.list.d/cloudflared.list

    echo "==> installing cloudflared"
    apt-get update
    apt-get install -y cloudflared

    echo
    echo "==> installed. Next: run this script's 'login-and-create' step as"
    echo "    yourself (NOT sudo — it opens a browser auth flow tied to your"
    echo "    own shell session):"
    echo "      ./setup-cloudflared.sh login-and-create"
    ;;

  login-and-create)
    if [[ $EUID -eq 0 ]]; then
      echo "run this step as yourself, not root/sudo — 'cloudflared tunnel login'" >&2
      echo "needs to open a browser auth flow and write credentials to YOUR home" >&2
      echo "directory, not root's." >&2
      exit 1
    fi
    if ! command -v cloudflared &>/dev/null; then
      echo "cloudflared not found — run 'sudo ./setup-cloudflared.sh install' first" >&2
      exit 1
    fi

    # --- Interactive steps, cannot be scripted blind ---
    # `cloudflared tunnel login` opens a browser (or prints a URL, if
    # this is a headless SSH session — copy it to a browser on your own
    # machine) and authenticates against YOUR Cloudflare account. There
    # is no flag to skip this — it's OAuth against Cloudflare's own
    # login, on purpose, since this is what proves you actually own the
    # zone you're about to route a tunnel to.
    echo "==> cloudflared tunnel login"
    echo "    (opens a browser / prints a URL — authenticate with the"
    echo "    Cloudflare account that owns the domain you're using)"
    cloudflared tunnel login

    echo "==> cloudflared tunnel create $TUNNEL_NAME"
    cloudflared tunnel create "$TUNNEL_NAME"

    echo
    echo "==> tunnel created. Route it to your real domain now — replace"
    echo "    the placeholder below with the actual hostname you're using"
    echo "    (this MUST match google_oauth_redirect_url's host once you"
    echo "    update config.toml — see this script's header comment):"
    echo
    echo "      cloudflared tunnel route dns $TUNNEL_NAME sniper.YOUR-DOMAIN.com"
    echo
    echo "    Then install the persistent service:"
    echo "      sudo ./setup-cloudflared.sh service"
    ;;

  service)
    if [[ $EUID -ne 0 ]]; then
      echo "run as root for 'service' (sudo ./setup-cloudflared.sh service)" >&2
      exit 1
    fi
    CRED_DIR="$(getent passwd "${SUDO_USER:-$USER}" | cut -d: -f6)/.cloudflared"
    if [[ ! -d "$CRED_DIR" ]] || [[ -z "$(find "$CRED_DIR" -maxdepth 1 -name '*.json' 2>/dev/null)" ]]; then
      echo "no tunnel credentials found in $CRED_DIR — run" >&2
      echo "'./setup-cloudflared.sh login-and-create' (as yourself, not root) first" >&2
      exit 1
    fi

    echo "==> writing /etc/cloudflared/config.yml"
    mkdir -p /etc/cloudflared
    CRED_FILE=$(find "$CRED_DIR" -maxdepth 1 -name '*.json' | head -1)
    TUNNEL_ID=$(basename "$CRED_FILE" .json)
    cp "$CRED_FILE" /etc/cloudflared/
    cat > /etc/cloudflared/config.yml <<EOF
tunnel: $TUNNEL_ID
credentials-file: /etc/cloudflared/$(basename "$CRED_FILE")
# STEP 16b — points at this bot's real bind address, confirmed against
# src/main.rs's API_BIND_ADDR before this script was written. The bot
# itself still binds 127.0.0.1 only (step 10.5b's own point stands
# unchanged by moving to a VPS): cloudflared reaches IN as a local
# client, it is never a reason to widen that bind address.
ingress:
  - service: $BOT_ADDR
EOF
    # No hostname routing rule in the ingress config above on purpose —
    # `cloudflared tunnel route dns` (run in login-and-create above)
    # already handles DNS routing for the hostname you chose; a
    # catch-all default service entry here is sufficient and keeps this
    # file from needing to duplicate that hostname a second place.

    echo "==> installing cloudflared as a systemd service"
    cloudflared service install
    systemctl daemon-reload
    systemctl enable --now cloudflared
    systemctl status cloudflared --no-pager

    echo
    echo "==> done. Verify from your phone per ui/README.md's 'Reaching this"
    echo "    from your phone' section — Cloudflare Access's own login page"
    echo "    should appear FIRST (once you've set up the Access application"
    echo "    in the Cloudflare dashboard yourself, per step 10.5c — this"
    echo "    script deliberately does not do that part), THEN this bot's"
    echo "    own Google/TOTP/WebAuthn flow."
    ;;

  *)
    echo "usage: $0 install | login-and-create | service" >&2
    echo "  install           (sudo)  installs cloudflared via Cloudflare's apt repo" >&2
    echo "  login-and-create  (as yourself) interactive browser auth + tunnel creation" >&2
    echo "  service           (sudo)  installs cloudflared as a persistent systemd service" >&2
    echo "Run in that order, once each. See this script's header comment before starting." >&2
    exit 1
    ;;
esac
