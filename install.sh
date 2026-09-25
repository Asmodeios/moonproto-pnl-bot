#!/usr/bin/env bash
# Installs (or updates) pnl-bot as a systemd service on Ubuntu, asks for the
# BotFather token on first install, and prints the link that claims the bot.
# Run from this folder: sudo ./install.sh
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  exec sudo -- "$0" "$@"
fi
cd "$(dirname "$0")"
trap 'echo; echo "Manage the bot with: pnl --help (start, stop, logs, ...)"' EXIT

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
install -m 0755 deploy/pnl-bot-passphrase.sh /usr/local/sbin/pnl-bot-passphrase
install -m 0755 deploy/pnl.sh /usr/local/bin/pnl

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

  echo
  echo "Choose a passphrase to encrypt the core keys you add. It is never stored: after every restart"
  echo "the bot asks you for it in Telegram. If you forget it, you add your cores again."
  systemctl stop pnl-bot 2>/dev/null || true
  /usr/local/sbin/pnl-bot-passphrase
fi
chmod 600 "$ENV_FILE"

install -m 0644 deploy/pnl-bot.service "$UNIT"
systemctl daemon-reload
systemctl enable pnl-bot >/dev/null 2>&1
# The bot writes where it stands to this file (src/status.rs); cleared so a
# stale one from an earlier run isn't read.
STATUS_FILE=/var/lib/private/pnl-bot/status
rm -f "$STATUS_FILE"
systemctl restart pnl-bot
field() { sed -n "s/^$1=//p" "$STATUS_FILE" 2>/dev/null | head -n 1 || true; }

echo "==> Starting"
for _ in $(seq 1 30); do
  case "$(field state)" in
    error)
      echo
      echo "pnl-bot stopped: $(field error)"
      echo "Fix it in $ENV_FILE (BOT_TOKEN), then run: pnl --restart"
      exit 1
      ;;
    unclaimed)
      echo
      echo "Almost done. Open this link in Telegram and press Start — it makes you the bot's owner:"
      echo
      echo "    $(field link)"
      echo
      echo "The link works once and only until the service restarts (a restart makes a new one:"
      echo "pnl --link)."
      if [ "$(field locked)" = yes ]; then
        echo "Then send the bot your passphrase to unlock it."
      fi
      exit 0
      ;;
    locked)
      echo
      echo "pnl-bot is running and locked. Open your bot in Telegram and send it your passphrase."
      exit 0
      ;;
    running)
      echo
      echo "pnl-bot is running and already has its owner. Open your bot in Telegram and send /menu."
      echo "The core keys are stored unencrypted; to encrypt them, run: pnl --passphrase"
      exit 0
      ;;
  esac
  sleep 1
done
echo
echo "The service did not report in. Check: pnl --logs 50"
exit 1
