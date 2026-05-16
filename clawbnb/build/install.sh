#!/usr/bin/env bash
#
# weclawbot Linux installer — fully one-button (v7.9).
#
#   curl -fsSL https://raw.githubusercontent.com/<repo>/main/clawbnb/build/install.sh | bash
#
# What this does (idempotent — safe to re-run as an upgrade path):
#
#   1. weclawbot binary  → ~/.local/bin/weclawbot (SHA-256 verified)
#   2. podman + runsc    → distro pkg / gvisor.dev release
#   3. Postgres 16       → podman container `weclawbot-pg` + systemd --user
#                          unit so it survives reboots. SKIPPED if
#                          $WECLAWBOT_PG_URL is already set in env or
#                          ~/.config/environment.d/weclawbot.conf.
#   4. Encryption key    → $HOME/.weclawbot/.db-key auto-gen (32 bytes
#                          urandom). If $WECLAWBOT_DB_KEY env is set,
#                          that wins.
#   5. environment.d wiring → systemd --user picks up PG_URL + DB_KEY
#   6. Sandbox image     → podman pull ghcr.io/<repo>/weclawbot-sandbox-base
#   7. weclawbot.service → systemd --user unit + linger so daemon
#                          survives logout. Started + enabled.
#
# Env knobs (all optional):
#   WECLAWBOT_REPO        — default tooichitake/weclawa-dvanced
#   WECLAWBOT_VERSION     — default latest
#   WECLAWBOT_PG_URL      — skip the podman PG container, use external PG
#   WECLAWBOT_DB_KEY      — skip key auto-gen, use this base64-32-byte key
#   BIN_DIR               — default ~/.local/bin
#   SKIP_POSTGRES         — set to 1 to skip step 3 entirely (advanced)
#   SKIP_IMAGE_PULL       — set to 1 to skip step 6 (offline air-gap install)
#   WECLAWBOT_SKIP_BINARY — set to 1 if you'll cargo-build the binary yourself

set -euo pipefail

REPO="${WECLAWBOT_REPO:-tooichitake/weclawa-dvanced}"
VERSION="${WECLAWBOT_VERSION:-latest}"
BIN_NAME="weclawbot"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
ENV_DIR="$HOME/.config/environment.d"
ENV_FILE="$ENV_DIR/weclawbot.conf"
STATE_DIR="$HOME/.weclawbot"

# Postgres container settings (overridable via env).
PG_CONTAINER_NAME="${PG_CONTAINER_NAME:-weclawbot-pg}"
PG_USER="${PG_USER:-weclawbot}"
PG_DB="${PG_DB:-weclawbot}"
PG_PORT="${PG_PORT:-5432}"
PG_IMAGE="${PG_IMAGE:-docker.io/postgres:16}"
PG_VOLUME="${PG_VOLUME:-weclawbot-pgdata}"

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

require_linux() {
    case "$(uname -s)" in
        Linux) ;;
        *) die "weclawbot only supports Linux. Got $(uname -s)." ;;
    esac
}

detect_distro() {
    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        echo "${ID:-unknown}"
    else
        echo "unknown"
    fi
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)  echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *) die "Unsupported architecture: $(uname -m)" ;;
    esac
}

SUDO=""
need_sudo() {
    if [ "$(id -u)" -ne 0 ]; then
        if ! command -v sudo >/dev/null 2>&1; then
            die "this step needs root, but sudo is not installed"
        fi
        SUDO="sudo"
    fi
}

# Generate `$1` random bytes, base64-encoded. Used for both
# WECLAWBOT_DB_KEY and the Postgres password.
rand_b64() {
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -base64 "$1"
    else
        head -c "$1" /dev/urandom | base64 | tr -d '\n'
    fi
}

# Append KEY=VALUE to environment.d if KEY not already there. mode 0600.
env_set_if_missing() {
    local key="$1"
    local value="$2"
    mkdir -p "$ENV_DIR"
    if [ -f "$ENV_FILE" ] && grep -q "^${key}=" "$ENV_FILE"; then
        log "  $ENV_FILE already has $key= — leaving as-is"
        return
    fi
    printf '%s=%s\n' "$key" "$value" >> "$ENV_FILE"
    chmod 600 "$ENV_FILE"
    log "  wrote $key to $ENV_FILE (mode 0600)"
}

# Read a key from environment.d (empty if missing).
env_get() {
    local key="$1"
    [ -f "$ENV_FILE" ] || { echo ""; return; }
    grep "^${key}=" "$ENV_FILE" 2>/dev/null | head -1 | cut -d= -f2-
}

# ----------- 1. weclawbot binary -----------

install_binary() {
    log "Step 1/7 — Installing weclawbot binary"
    if [ "${WECLAWBOT_SKIP_BINARY:-0}" = "1" ]; then
        log "  WECLAWBOT_SKIP_BINARY=1 — skipping (assumed already in $BIN_DIR)"
        return
    fi
    mkdir -p "$BIN_DIR"

    local arch tag asset url
    arch="$(detect_arch)"
    if [ "$VERSION" = "latest" ]; then
        tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
               | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)" || true
        if [ -z "$tag" ]; then
            warn "No GitHub Release published yet for $REPO."
            warn "Build locally with:"
            warn "  cd clawbnb && cargo build --release"
            warn "  cp target/release/weclawbot $BIN_DIR/"
            warn "Then re-run install.sh with WECLAWBOT_SKIP_BINARY=1"
            die "no release tag found for $REPO"
        fi
    else
        tag="$VERSION"
    fi
    asset="weclawbot-linux-${arch}"
    url="https://github.com/$REPO/releases/download/$tag/$asset"

    log "  download $url"
    local tmp
    tmp="$(mktemp -d)"
    curl -fsSL -o "$tmp/$asset" "$url"

    # SHA-256 verification — Release CI generates SHA256SUMS file (v7.9+).
    # Releases without one: warn but proceed.
    if curl -fsSL -o "$tmp/SHA256SUMS" \
        "https://github.com/$REPO/releases/download/$tag/SHA256SUMS" 2>/dev/null; then
        log "  verifying sha256"
        ( cd "$tmp" && grep " $asset\$" SHA256SUMS | sha256sum -c - ) \
            || die "sha256 mismatch — release tampered or partial download"
    else
        warn "  no SHA256SUMS published for $tag — skipping checksum verify"
    fi

    install -m 0755 "$tmp/$asset" "$BIN_DIR/$BIN_NAME"
    rm -rf "$tmp"

    if ! printf '%s' "$PATH" | tr ':' '\n' | grep -qx "$BIN_DIR"; then
        warn "$BIN_DIR is not in PATH. Add to ~/.bashrc / ~/.zshrc:"
        warn "  export PATH=\"$BIN_DIR:\$PATH\""
    fi

    "$BIN_DIR/$BIN_NAME" version
}

# ----------- 2. podman -----------

install_podman() {
    log "Step 2/7 — podman"
    if command -v podman >/dev/null 2>&1; then
        log "  already installed: $(podman --version)"
        return
    fi
    need_sudo
    case "$(detect_distro)" in
        ubuntu|debian)
            $SUDO apt-get update -qq
            $SUDO apt-get install -y podman
            ;;
        fedora|rhel|centos|rocky|almalinux)
            $SUDO dnf install -y podman
            ;;
        arch|endeavouros|manjaro)
            $SUDO pacman -Sy --noconfirm podman
            ;;
        opensuse-leap|opensuse-tumbleweed)
            $SUDO zypper install -y podman
            ;;
        *)
            die "Unsupported distro for automatic podman install — install manually then re-run."
            ;;
    esac
    log "  installed: $(podman --version)"
}

# ----------- 3. Postgres (podman container) -----------

install_postgres() {
    log "Step 3/7 — Postgres"
    if [ "${SKIP_POSTGRES:-0}" = "1" ]; then
        log "  SKIP_POSTGRES=1 — skipping"
        return
    fi
    # User already has a DSN configured? Honor it.
    local existing
    existing="$(env_get WECLAWBOT_PG_URL)"
    if [ -n "$existing" ] || [ -n "${WECLAWBOT_PG_URL:-}" ]; then
        log "  WECLAWBOT_PG_URL already configured — skipping podman PG"
        return
    fi

    # Generate a random password, sanitize for URL safety.
    local pg_pw dsn
    pg_pw="$(rand_b64 24)"
    pg_pw="${pg_pw//\//_}"
    pg_pw="${pg_pw//+/-}"
    pg_pw="${pg_pw//=/}"
    dsn="postgres://${PG_USER}:${pg_pw}@127.0.0.1:${PG_PORT}/${PG_DB}"

    # If the container already exists (re-run install), just (re)start.
    if podman container exists "$PG_CONTAINER_NAME" 2>/dev/null; then
        log "  container $PG_CONTAINER_NAME exists — starting"
        podman start "$PG_CONTAINER_NAME" >/dev/null
        warn "  re-using existing container — its PASSWORD is unchanged"
        warn "  if WECLAWBOT_PG_URL got lost, exec into container and reset:"
        warn "    podman exec -it $PG_CONTAINER_NAME psql -U postgres -c \"ALTER USER $PG_USER WITH PASSWORD '...';\""
        # Don't write a fresh DSN — operator must reuse the original or reset.
        return
    fi

    log "  podman pull $PG_IMAGE"
    podman pull "$PG_IMAGE" >/dev/null
    log "  creating container $PG_CONTAINER_NAME (volume=$PG_VOLUME)"
    podman run -d \
        --name "$PG_CONTAINER_NAME" \
        --restart unless-stopped \
        -e POSTGRES_USER="$PG_USER" \
        -e POSTGRES_PASSWORD="$pg_pw" \
        -e POSTGRES_DB="$PG_DB" \
        -p "127.0.0.1:${PG_PORT}:5432" \
        -v "${PG_VOLUME}:/var/lib/postgresql/data" \
        "$PG_IMAGE" >/dev/null

    log "  waiting for Postgres to be ready (≤60s)"
    local i=0
    until podman exec "$PG_CONTAINER_NAME" pg_isready -U "$PG_USER" >/dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -ge 60 ]; then
            die "Postgres failed to come up — check 'podman logs $PG_CONTAINER_NAME'"
        fi
        sleep 1
    done
    log "  ready"

    env_set_if_missing WECLAWBOT_PG_URL "$dsn"
    install_postgres_systemd_unit
}

# Make the PG container survive reboots without relying on podman's
# `--restart unless-stopped` (which only works while the user is
# logged in on rootless podman). Backstop via systemd --user unit.
install_postgres_systemd_unit() {
    local unit="$HOME/.config/systemd/user/${PG_CONTAINER_NAME}.service"
    mkdir -p "$(dirname "$unit")"
    cat > "$unit" <<EOF
[Unit]
Description=Postgres 16 container for weclawbot (managed by install.sh)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Restart=on-failure
RestartSec=5s
# Idempotent start — if the container exists, this just re-attaches.
ExecStart=/usr/bin/podman start --attach $PG_CONTAINER_NAME
ExecStop=/usr/bin/podman stop -t 30 $PG_CONTAINER_NAME

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload || true
    systemctl --user enable --now "${PG_CONTAINER_NAME}.service" 2>/dev/null || \
        warn "could not enable ${PG_CONTAINER_NAME}.service — start manually with 'podman start $PG_CONTAINER_NAME'"
}

# ----------- 4. Encryption key -----------

install_db_key() {
    log "Step 4/7 — Encryption key"
    local existing
    existing="$(env_get WECLAWBOT_DB_KEY)"
    if [ -n "$existing" ] || [ -n "${WECLAWBOT_DB_KEY:-}" ]; then
        log "  WECLAWBOT_DB_KEY already configured — leaving as-is"
        return
    fi
    # If a key file already exists on disk, daemon will pick it up.
    if [ -f "$STATE_DIR/.db-key" ]; then
        log "  $STATE_DIR/.db-key already exists — daemon will use it"
        return
    fi
    local key
    key="$(rand_b64 32 | tr -d '\n')"
    env_set_if_missing WECLAWBOT_DB_KEY "$key"
    warn "  BACK UP THIS KEY: $ENV_FILE contains WECLAWBOT_DB_KEY"
    warn "  Losing it makes encrypted tokens / SSO cookies / audit"
    warn "  archives unrecoverable."
}

# ----------- 5. runsc (gVisor) -----------

install_runsc() {
    log "Step 5/7 — runsc (gVisor)"
    if command -v runsc >/dev/null 2>&1; then
        log "  already installed: $(runsc --version 2>&1 | head -1)"
    else
        need_sudo
        local arch
        case "$(uname -m)" in
            x86_64) arch="x86_64" ;;
            aarch64) arch="arm64" ;;
            *) die "gVisor only ships for x86_64 and arm64" ;;
        esac
        local tmp
        tmp="$(mktemp -d)"
        log "  download runsc + sha512"
        curl -fsSL "https://storage.googleapis.com/gvisor/releases/release/latest/${arch}/runsc" \
             -o "$tmp/runsc"
        curl -fsSL "https://storage.googleapis.com/gvisor/releases/release/latest/${arch}/runsc.sha512" \
             -o "$tmp/runsc.sha512"
        ( cd "$tmp" && sha512sum -c runsc.sha512 ) || die "runsc checksum mismatch"
        chmod +x "$tmp/runsc"
        $SUDO mv "$tmp/runsc" /usr/local/bin/runsc
        rm -rf "$tmp"
        log "  installed to /usr/local/bin/runsc"
    fi

    log "  registering runsc with podman"
    mkdir -p "$HOME/.config/containers"
    local conf="$HOME/.config/containers/containers.conf"
    if [ -f "$conf" ] && grep -q '^runsc *=' "$conf" 2>/dev/null; then
        log "  already registered"
        return
    fi
    if [ -f "$conf" ] && grep -q '^\[engine.runtimes\]' "$conf"; then
        sed -i '/^\[engine.runtimes\]/a runsc = ["/usr/local/bin/runsc"]' "$conf"
    else
        cat >> "$conf" <<EOF

[engine.runtimes]
runsc = ["/usr/local/bin/runsc"]
EOF
    fi
}

# ----------- 6. Sandbox image -----------

pull_image() {
    log "Step 6/7 — Sandbox base image"
    if [ "${SKIP_IMAGE_PULL:-0}" = "1" ]; then
        log "  SKIP_IMAGE_PULL=1 — skipping"
        return
    fi
    local image="ghcr.io/${REPO}/weclawbot-sandbox-base:latest"
    log "  podman pull $image"
    if ! podman pull "$image" 2>/dev/null; then
        warn "  pull failed — image may not be published yet, or you're offline."
        warn "  daemon will retry on first inbound message."
        warn "  manually: podman pull $image"
    fi
}

# ----------- 7. systemd user service -----------

install_systemd_unit() {
    log "Step 7/7 — systemd --user unit"
    mkdir -p "$HOME/.config/systemd/user"
    local unit="$HOME/.config/systemd/user/weclawbot.service"
    cat > "$unit" <<EOF
[Unit]
Description=weclawbot — WeChat ↔ Claude Code bridge
After=network-online.target ${PG_CONTAINER_NAME}.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=$BIN_DIR/$BIN_NAME start --foreground
Restart=on-failure
RestartSec=5s
# environment.d already wires WECLAWBOT_PG_URL + WECLAWBOT_DB_KEY,
# but a defensive explicit EnvironmentFile= also works for older
# systemd versions that don't honor environment.d for user units
# (pre-247). The '-' prefix makes it optional (no failure if absent).
EnvironmentFile=-$ENV_FILE
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF

    systemctl --user daemon-reload || true

    # Enable linger so daemon survives logout (no-op when run as root).
    if command -v loginctl >/dev/null 2>&1 && [ "$(id -u)" -ne 0 ]; then
        need_sudo
        $SUDO loginctl enable-linger "$USER" >/dev/null 2>&1 || \
            warn "loginctl enable-linger failed — daemon will die at user logout"
    fi

    log "  enable + start"
    systemctl --user enable --now weclawbot.service || \
        warn "service didn't start cleanly — check 'journalctl --user -u weclawbot --since 5min ago'"
}

# ----------- Main -----------

main() {
    require_linux
    log "weclawbot install starting"
    log "  repo:      $REPO"
    log "  version:   $VERSION"
    log "  bin_dir:   $BIN_DIR"
    log "  env_file:  $ENV_FILE"
    log ""

    install_binary
    install_podman
    install_postgres
    install_db_key
    install_runsc
    pull_image
    install_systemd_unit

    log ""
    log "Running doctor"
    "$BIN_DIR/$BIN_NAME" doctor || warn "doctor reported issues — review above"

    log ""
    log "✓ install complete."
    log ""
    log "Next steps:"
    log "  1. Find the INITIAL-ADMIN-KEY in the daemon log (printed ONCE):"
    log "       journalctl --user -u weclawbot --since 5min ago | grep INITIAL-ADMIN-KEY"
    log "  2. Visit the admin console: http://127.0.0.1:18011"
    log "  3. Bind a WeChat account: $BIN_DIR/$BIN_NAME login"
    log ""
    log "Operations:"
    log "  status:    systemctl --user status weclawbot"
    log "  logs:      journalctl --user -u weclawbot -f"
    log "  restart:   systemctl --user restart weclawbot"
    log "  uninstall: ./uninstall.sh  (--purge to also wipe ~/.weclawbot data)"
    log ""
    log "  ⚠ BACK UP $ENV_FILE — it contains WECLAWBOT_DB_KEY."
    log "    Losing the key makes encrypted data unrecoverable."
}

main "$@"
