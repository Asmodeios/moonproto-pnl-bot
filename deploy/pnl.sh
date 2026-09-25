#!/usr/bin/env bash
# Controls the pnl-bot service. install.sh puts it at /usr/local/bin/pnl.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: pnl <command>

  --start        Start the bot
  --stop         Stop the bot
  --restart      Restart the bot (a locked bot asks for the passphrase again)
  --status       Show whether the bot is running
  --logs         Follow the log (Ctrl+C to leave); --logs N prints the last N lines
  --link         Print the claim link, while the bot has no owner
  --passphrase   Set, change or remove the passphrase on the core keys
  --help         Show this help
EOF
}

CMD="${1:---help}"
case "$CMD" in
  -h|--help|help) usage; exit 0 ;;
esac

if [ "$(id -u)" -ne 0 ]; then
  exec sudo -- "$0" "$@"
fi

case "$CMD" in
  --start|--restart)
    systemctl "${CMD#--}" pnl-bot
    systemctl --no-pager --lines=0 status pnl-bot | head -n 3
    ;;
  --stop)
    systemctl stop pnl-bot
    echo "pnl-bot stopped."
    ;;
  --status)
    systemctl --no-pager --lines=10 status pnl-bot || true
    ;;
  --logs)
    if [ -n "${2:-}" ]; then
      [[ "$2" =~ ^[0-9]+$ ]] || { echo "--logs takes a number of lines, e.g. pnl --logs 100" >&2; exit 2; }
      journalctl -u pnl-bot -n "$2" -o cat --no-pager
    else
      journalctl -u pnl-bot -n 50 -o cat -f
    fi
    ;;
  --link)
    # Where the bot writes where it stands (src/status.rs).
    STATUS_FILE=/var/lib/private/pnl-bot/status
    if ! systemctl is-active --quiet pnl-bot; then
      echo "pnl-bot isn't running. Start it with: pnl --start"
      exit 1
    fi
    if grep -qx 'state=unclaimed' "$STATUS_FILE" 2>/dev/null; then
      sed -n 's/^link=//p' "$STATUS_FILE"
      echo "Open it in Telegram and press Start to become the bot's owner. It works until the next restart."
    else
      echo "No claim link: the bot already has its owner."
    fi
    ;;
  --passphrase)
    exec /usr/local/sbin/pnl-bot-passphrase
    ;;
  *)
    echo "Unknown command: $CMD" >&2
    echo >&2
    usage >&2
    exit 2
    ;;
esac
