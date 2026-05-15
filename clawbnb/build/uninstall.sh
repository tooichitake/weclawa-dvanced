#!/usr/bin/env bash
#
# Remove weclawbot. Leaves your ~/.claude/ alone; only removes things
# install.sh added.
#
#   bash uninstall.sh           # interactive
#   bash uninstall.sh --purge   # also wipe ~/.weclawbot/

set -euo pipefail

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

log "Stopping weclawbot service"
systemctl --user stop weclawbot 2>/dev/null || true
systemctl --user disable weclawbot 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/weclawbot.service"
systemctl --user daemon-reload 2>/dev/null || true

log "Removing weclawbot binary"
rm -f "$HOME/.local/bin/weclawbot"

if [ "$PURGE" -eq 1 ]; then
    log "Purging ~/.weclawbot/ (sandbox state, history, media)"
    rm -rf "$HOME/.weclawbot"
fi

log "Done. podman, runsc, and ~/.claude/ left in place."
