#!/usr/bin/env bash
#
# Remove weclawbot. Leaves your ~/.claude/ alone; only removes things
# install.sh added.
#
#   bash uninstall.sh           # interactive (preserves ~/.weclawbot data)
#   bash uninstall.sh --purge   # also wipe ~/.weclawbot/ AND drop the
#                                 podman Postgres container + volume

set -euo pipefail

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

PG_CONTAINER_NAME="${PG_CONTAINER_NAME:-weclawbot-pg}"
PG_VOLUME="${PG_VOLUME:-weclawbot-pgdata}"
ENV_FILE="$HOME/.config/environment.d/weclawbot.conf"

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }

log "Stopping weclawbot service"
systemctl --user stop weclawbot 2>/dev/null || true
systemctl --user disable weclawbot 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/weclawbot.service"

log "Removing weclawbot binary"
rm -f "$HOME/.local/bin/weclawbot"

# v7.9 — Postgres container + systemd unit. We installed both as part
# of one-button install; uninstall mirrors. The container itself stays
# UNLESS --purge (data integrity by default; `--purge` is the operator
# saying "really wipe everything").
log "Stopping Postgres container service"
systemctl --user stop "${PG_CONTAINER_NAME}.service" 2>/dev/null || true
systemctl --user disable "${PG_CONTAINER_NAME}.service" 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/${PG_CONTAINER_NAME}.service"

if command -v podman >/dev/null 2>&1; then
    # Stop the container (preserves data). --purge below drops it
    # entirely.
    podman stop "$PG_CONTAINER_NAME" 2>/dev/null || true
fi

systemctl --user daemon-reload 2>/dev/null || true

if [ "$PURGE" -eq 1 ]; then
    log "Purging ~/.weclawbot/ (sandbox state, history, media, .db-key)"
    rm -rf "$HOME/.weclawbot"

    log "Purging environment.d file ($ENV_FILE)"
    rm -f "$ENV_FILE"

    if command -v podman >/dev/null 2>&1; then
        log "Removing Postgres container $PG_CONTAINER_NAME"
        podman rm -f "$PG_CONTAINER_NAME" 2>/dev/null || true
        log "Removing Postgres volume $PG_VOLUME"
        podman volume rm -f "$PG_VOLUME" 2>/dev/null || true
    fi
fi

log "Done."
log "  podman, runsc, claude CLI, ~/.claude/ left in place."
if [ "$PURGE" -eq 0 ]; then
    log "  Postgres container preserved — re-running install.sh restores service."
    log "  Pass --purge to also wipe ~/.weclawbot + container + volume."
fi
