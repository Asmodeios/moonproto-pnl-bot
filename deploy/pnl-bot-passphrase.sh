#!/usr/bin/env bash
# Sets, changes or removes the passphrase that encrypts the core keys.
# Stops the service while cores.json is rewritten, then starts it again.
# install.sh puts it at /usr/local/sbin/pnl-bot-passphrase; run: sudo pnl-bot-passphrase
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  exec sudo -- "$0" "$@"
fi

# Where systemd keeps StateDirectory= for a DynamicUser= service.
STATE=/var/lib/private/pnl-bot
install -d -m 0700 /var/lib/private "$STATE"

RUNNING=0
if systemctl is-active --quiet pnl-bot; then
  RUNNING=1
  systemctl stop pnl-bot
fi

STATUS=0
DATA_DIR="$STATE" /opt/pnl-bot/pnl-bot passphrase || STATUS=$?
# Written as root: hand it to the service's user. A new directory is still
# root's, and systemd hands it over on the service's first start.
chown -R --reference="$STATE" "$STATE"

if [ "$RUNNING" -eq 1 ]; then
  systemctl start pnl-bot
fi
exit "$STATUS"
