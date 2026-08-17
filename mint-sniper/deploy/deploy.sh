#!/usr/bin/env bash
# STEP 15b — first-deploy / redeploy script for a fresh VPS.
# See ../DEPLOY.md for the full step-by-step checklist this is one piece
# of (provisioning, DNS, Cloudflare Access, wallet funding — none of
# that happens here). This script ONLY builds/updates the code and
# (re)starts the systemd service; it never touches secrets, config.toml,
# or the identity DB.
#
# Usage:
#   sudo ./deploy.sh source   # git clone/pull + cargo build --release + npm build (default)
#   sudo ./deploy.sh release  # download the latest GitHub release tarball instead
#
# Idempotent: safe to re-run for an update (git pull + rebuild, or a
# newer release tarball) — existing config.toml/.sniper-token/
# identity.db/mint-sniper.env are never touched or overwritten.

set -euo pipefail

MODE="${1:-source}"
REPO_URL="https://github.com/Matthias141/seasniper.git"
INSTALL_DIR="/opt/mint-sniper"
SERVICE_USER="mint-sniper"

if [[ $EUID -ne 0 ]]; then
  echo "run as root (sudo ./deploy.sh)" >&2
  exit 1
fi

# --- one-time setup: dedicated unprivileged user + install dir ---
if ! id "$SERVICE_USER" &>/dev/null; then
  echo "==> creating system user $SERVICE_USER"
  useradd --system --create-home --home-dir "$INSTALL_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
fi
mkdir -p "$INSTALL_DIR"

case "$MODE" in
  source)
    echo "==> building from source"
    if [[ ! -d "$INSTALL_DIR/repo/.git" ]]; then
      sudo -u "$SERVICE_USER" git clone "$REPO_URL" "$INSTALL_DIR/repo"
    else
      sudo -u "$SERVICE_USER" git -C "$INSTALL_DIR/repo" fetch origin
      # Deliberately fixed to main, not whatever branch happened to be
      # checked out last — a deploy should always ship what's actually
      # merged, never an in-progress feature branch by accident.
      sudo -u "$SERVICE_USER" git -C "$INSTALL_DIR/repo" checkout main
      sudo -u "$SERVICE_USER" git -C "$INSTALL_DIR/repo" pull --ff-only origin main
    fi

    if ! command -v cargo &>/dev/null; then
      echo "cargo not found — install the Rust toolchain first (see DEPLOY.md step 2)" >&2
      exit 1
    fi
    if ! command -v npm &>/dev/null; then
      echo "npm not found — install Node.js first (see DEPLOY.md step 2)" >&2
      exit 1
    fi

    echo "==> cargo build --release (this takes a few minutes on a small VPS)"
    sudo -u "$SERVICE_USER" bash -c "cd '$INSTALL_DIR/repo/mint-sniper' && cargo build --release"

    echo "==> npm ci && npm run build"
    sudo -u "$SERVICE_USER" bash -c "cd '$INSTALL_DIR/repo/mint-sniper/ui' && npm ci && npm run build"

    echo "==> linking binary + ui/dist into $INSTALL_DIR"
    ln -sf "$INSTALL_DIR/repo/mint-sniper/target/release/mint-sniper" "$INSTALL_DIR/mint-sniper"
    rm -rf "$INSTALL_DIR/ui"
    mkdir -p "$INSTALL_DIR/ui"
    ln -sf "$INSTALL_DIR/repo/mint-sniper/ui/dist" "$INSTALL_DIR/ui/dist"
    ;;

  release)
    echo "==> fetching latest GitHub release"
    LATEST_URL=$(curl -sS https://api.github.com/repos/Matthias141/seasniper/releases/latest \
      | grep '"browser_download_url"' | grep '\.tar\.gz"' | head -1 | cut -d '"' -f4)
    if [[ -z "$LATEST_URL" ]]; then
      echo "no release tarball found — has a v* tag been pushed yet? falling back to source mode may be needed." >&2
      exit 1
    fi
    TMP=$(mktemp -d)
    curl -sSL "$LATEST_URL" -o "$TMP/release.tar.gz"
    tar xzf "$TMP/release.tar.gz" -C "$TMP"
    STAGE_DIR=$(find "$TMP" -maxdepth 1 -type d -name 'mint-sniper-*' | head -1)
    install -m 755 -o "$SERVICE_USER" -g "$SERVICE_USER" "$STAGE_DIR/mint-sniper" "$INSTALL_DIR/mint-sniper"
    # The release tarball names this directory "ui-dist" (release.yml's
    # own packaging step) — the binary expects "ui/dist" relative to its
    # CWD (api.rs's ServeDir fallback, step 15b). Reconciled here rather
    # than in release.yml, since renaming there would also require
    # re-verifying that workflow end-to-end by cutting a real tag.
    rm -rf "$INSTALL_DIR/ui"
    mkdir -p "$INSTALL_DIR/ui"
    cp -r "$STAGE_DIR/ui-dist" "$INSTALL_DIR/ui/dist"
    chown -R "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR/ui"
    rm -rf "$TMP"
    ;;

  *)
    echo "unknown mode '$MODE' — use 'source' or 'release'" >&2
    exit 1
    ;;
esac

# --- systemd unit (safe to re-copy on every deploy; doesn't touch secrets) ---
echo "==> installing systemd unit"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cp "$SCRIPT_DIR/mint-sniper.service" /etc/systemd/system/mint-sniper.service
systemctl daemon-reload

echo
echo "==> deploy staged. NOT starting the service yet — see DEPLOY.md for the"
echo "    remaining one-time steps (config.toml, mint-sniper.env, funding"
echo "    wallets) before running:"
echo "      sudo systemctl enable --now mint-sniper"
echo "    On an UPDATE (config.toml/mint-sniper.env already in place), just:"
echo "      sudo systemctl restart mint-sniper"
