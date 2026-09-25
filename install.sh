#!/usr/bin/env bash
# Installs (or updates) pnl-bot as a systemd service on Ubuntu, asks for the
# BotFather token on first install, and prints the link that claims the bot.
# Run from this folder: sudo ./install.sh
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  exec sudo -- "$0" "$@"
fi
cd "$(dirname "$0")"

ENV_FILE=/etc/pnl-bot.env
UNIT=/etc/systemd/system/pnl-bot.service

# A release archive ships the binary next to this script; a source checkout builds it.
if [ -x ./pnl-bot ] && [ ! -f Cargo.toml ]; then
  BIN=./pnl-bot
else
  if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    echo "==> Installing the Rust toolchain"
    apt-get update -q
    apt-get install -y -q build-essential pkg-config curl ca-certificates
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  fi
  export PATH="$HOME/.cargo/bin:$PATH"
  echo "==> Building (the first build takes several minutes)"
  cargo build --release --locked 2>/dev/null || cargo build --release
  BIN=target/release/pnl-bot
fi

install -D -m 0755 "$BIN" /opt/pnl-bot/pnl-bot

if ! grep -qs '^BOT_TOKEN=.' "$ENV_FILE"; then
  echo
  echo "Create a bot with @BotFather in Telegram (/newbot) and paste its token."
  while true; do
    read -rsp "Bot token: " TOKEN
    echo
    if [[ "$TOKEN" =~ ^[0-9]+:[A-Za-z0-9_-]{30,}$ ]]; then
      break
    fi
    echo "That doesn't look like a bot token (it looks like 123456789:AA...). Try again."
  done
  (
    umask 077
    printf 'BOT_TOKEN=%s\nREPORT_UTC_OFFSET_MINUTES=0\n' "$TOKEN" > "$ENV_FILE"
  )
  unset TOKEN
fi
chmod 600 "$ENV_FILE"

install -m 0644 deploy/pnl-bot.service "$UNIT"
systemctl daemon-reload
START="$(date '+%Y-%m-%d %H:%M:%S')"
systemctl enable pnl-bot >/dev/null 2>&1
systemctl restart pnl-bot

echo "==> Starting"
for _ in $(seq 1 30); do
  LOG="$(journalctl -u pnl-bot --since "$START" -o cat --no-pager 2>/dev/null || true)"
  if grep -q 'bot token was not accepted' <<<"$LOG"; then
    echo
    echo "Telegram rejected the token. Fix BOT_TOKEN in $ENV_FILE, then run: sudo systemctl restart pnl-bot"
    exit 1
  fi
  LINK="$(grep -o 'https://t\.me/[^ ]*' <<<"$LOG" | tail -n 1 || true)"
  if [ -n "$LINK" ]; then
    echo
    echo "Almost done. Open this link in Telegram and press Start — it makes you the bot's owner:"
    echo
    echo "    $LINK"
    echo
    echo "The link works once and only until the service restarts (a restart prints a new one:"
    echo "journalctl -u pnl-bot | grep t.me)."
    exit 0
  fi
  if grep -q 'started with' <<<"$LOG"; then
    echo
    echo "pnl-bot is running and already has its owner. Open your bot in Telegram and send /menu."
    exit 0
  fi
  sleep 1
done
echo
echo "The service did not report in. Check: journalctl -u pnl-bot -n 50"
exit 1
