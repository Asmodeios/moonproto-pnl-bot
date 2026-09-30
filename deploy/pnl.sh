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
  --update       Download and install the latest release (--update --force reinstalls it)
  --help         Show this help
EOF
}

# Also `repository` in Cargo.toml, which the bot checks for new releases.
REPO=Asmodeios/moonproto-pnl-bot

# Downloads the latest release for this CPU, checks it against the release's
# SHA256SUMS and runs its install.sh, which keeps the token, owner and cores
# and restarts the service.
update() {
  # tmp stays global: the EXIT trap reads it after this function has returned.
  local arch tag current archive base
  case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) echo "There is no release for this CPU ($(uname -m)). Build from source instead." >&2; exit 1 ;;
  esac
  for tool in curl tar sha256sum; do
    command -v "$tool" >/dev/null || { echo "pnl --update needs $tool: apt install $tool" >&2; exit 1; }
  done

  # /releases/latest redirects to the newest release's tag page.
  tag="$(curl -fsSLI --proto '=https' -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" \
    || { echo "Could not reach GitHub." >&2; exit 1; }
  tag="${tag##*/}"
  if ! [[ "$tag" =~ ^v[0-9] ]]; then
    echo "No release found on https://github.com/$REPO/releases" >&2
    exit 1
  fi
  # Written by install.sh; missing on installs older than this command.
  current="$(cat /opt/pnl-bot/VERSION 2>/dev/null || true)"
  if [ "v$current" = "$tag" ] && [ "${1:-}" != --force ]; then
    echo "pnl-bot $current is the latest version. To reinstall it: pnl --update --force"
    return
  fi

  echo "==> Downloading pnl-bot $tag ($arch)${current:+, installed: v$current}"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  archive="pnl-bot-linux-$arch.tar.gz"
  base="https://github.com/$REPO/releases/download/$tag"
  curl -fsSL --proto '=https' -o "$tmp/$archive" "$base/$archive"
  curl -fsSL --proto '=https' -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
  if ! (cd "$tmp" && grep " $archive\$" SHA256SUMS | sha256sum -c --quiet -); then
    echo "The download does not match SHA256SUMS. Nothing was installed." >&2
    exit 1
  fi
  tar -xzf "$tmp/$archive" -C "$tmp"
  "$tmp/pnl-bot/install.sh"
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
  --update)
    update "${2:-}"
    ;;
  *)
    echo "Unknown command: $CMD" >&2
    echo >&2
    usage >&2
    exit 2
    ;;
esac
