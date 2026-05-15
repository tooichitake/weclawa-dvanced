#!/usr/bin/env bash
#
# weclawbot installer for Linux servers.
#
#   curl -fsSL https://raw.githubusercontent.com/<repo>/main/clawbnb/build/install.sh | bash
#
# Installs:
#   1. weclawbot binary from GitHub Releases
#   2. podman (container runtime, daemonless)
#   3. runsc (gVisor user-space kernel) and registers it as a podman runtime
#   4. The weclawbot sandbox base image (~800MB pull)
#   5. systemd --user service unit
#
# Enables linger so the daemon survives logout.
#
# This script is intentionally idempotent — re-running upgrades each piece in place.

set -euo pipefail

REPO="${WECLAWBOT_REPO:-anthropics/weclawa-advanced}"
VERSION="${WECLAWBOT_VERSION:-latest}"
BIN_NAME="weclawbot"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"

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

# ----------- 1. weclawbot binary -----------

install_binary() {
    log "Installing weclawbot binary"
    mkdir -p "$BIN_DIR"

    local arch tag asset url
    arch="$(detect_arch)"
    if [ "$VERSION" = "latest" ]; then
        tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
               | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
        [ -n "$tag" ] || die "could not resolve latest weclawbot release tag"
    else
        tag="$VERSION"
    fi
    asset="weclawbot-linux-${arch}"
    url="https://github.com/$REPO/releases/download/$tag/$asset"

    log "  download $url"
    curl -fsSL -o "$BIN_DIR/$BIN_NAME" "$url"
    chmod +x "$BIN_DIR/$BIN_NAME"

    if ! grep -q "$BIN_DIR" <<<"$PATH"; then
        warn "$BIN_DIR is not in PATH — add to your shell rc:"
        warn "  export PATH=\"$BIN_DIR:\$PATH\""
    fi

    "$BIN_DIR/$BIN_NAME" version
}

# ----------- 2. podman -----------

install_podman() {
    if command -v podman >/dev/null 2>&1; then
        log "podman already installed: $(podman --version)"
        return
    fi
    log "Installing podman"
    need_sudo
    case "$(detect_distro)" in
        ubuntu|debian) $SUDO apt-get update && $SUDO apt-get install -y podman ;;
        fedora|rhel|centos|rocky|almalinux) $SUDO dnf install -y podman ;;
        arch) $SUDO pacman -S --noconfirm podman ;;
        *) die "Unsupported distro for automatic podman install — install manually then re-run." ;;
    esac
}

# ----------- 3. runsc (gVisor) -----------

install_runsc() {
    if command -v runsc >/dev/null 2>&1; then
        log "runsc already installed: $(runsc --version | head -1)"
    else
        log "Installing runsc (gVisor)"
        need_sudo
        local arch
        case "$(uname -m)" in
            x86_64) arch="x86_64" ;;
            aarch64) arch="arm64" ;;
            *) die "gVisor only ships for x86_64 and arm64" ;;
        esac
        local tmp
        tmp="$(mktemp -d)"
        # Pinning to a known-good release tag would go here; use the latest
        # rolling release for now.
        curl -fsSL "https://storage.googleapis.com/gvisor/releases/release/latest/${arch}/runsc" \
             -o "$tmp/runsc"
        curl -fsSL "https://storage.googleapis.com/gvisor/releases/release/latest/${arch}/runsc.sha512" \
             -o "$tmp/runsc.sha512"
        ( cd "$tmp" && sha512sum -c runsc.sha512 ) || die "runsc checksum mismatch"
        chmod +x "$tmp/runsc"
        $SUDO mv "$tmp/runsc" /usr/local/bin/runsc
        rm -rf "$tmp"
    fi

    log "Registering runsc with podman"
    mkdir -p "$HOME/.config/containers"
    local conf="$HOME/.config/containers/containers.conf"
    if [ -f "$conf" ] && grep -q '^\[engine.runtimes\]' "$conf"; then
        if ! grep -q '^runsc *=' "$conf"; then
            # Append under the existing [engine.runtimes] section. Naive sed,
            # works because we control the file format we just wrote.
            sed -i '/^\[engine.runtimes\]/a runsc = ["/usr/local/bin/runsc"]' "$conf"
        fi
    else
        cat >> "$conf" <<EOF

[engine.runtimes]
runsc = ["/usr/local/bin/runsc"]
EOF
    fi
}

# ----------- 4. sandbox base image -----------

pull_image() {
    local image="ghcr.io/${REPO}/weclawbot-sandbox-base:latest"
    log "Pulling sandbox base image: $image"
    if ! podman pull "$image"; then
        warn "image pull failed; daemon will retry on first message"
    fi
}

# ----------- 5. systemd user service -----------

install_systemd_unit() {
    log "Installing systemd --user unit"
    mkdir -p "$HOME/.config/systemd/user"
    local unit="$HOME/.config/systemd/user/weclawbot.service"
    cat > "$unit" <<EOF
[Unit]
Description=weclawbot — WeChat ↔ Claude Code bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$BIN_DIR/$BIN_NAME start --foreground
Restart=on-failure
RestartSec=5s
# Logs go via journald (journalctl --user -u weclawbot).
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF

    systemctl --user daemon-reload || true

    # Enable linger so service survives user logout.
    if command -v loginctl >/dev/null 2>&1; then
        need_sudo
        $SUDO loginctl enable-linger "$USER" || warn "loginctl enable-linger failed"
    fi

    log "  enable + start"
    systemctl --user enable --now weclawbot.service
}

main() {
    require_linux
    install_binary
    install_podman
    install_runsc
    pull_image
    install_systemd_unit

    log "Running doctor"
    "$BIN_DIR/$BIN_NAME" doctor || warn "doctor reported issues — see above"

    log "Done."
    log "  Logs:    journalctl --user -u weclawbot -f"
    log "  Status:  systemctl --user status weclawbot"
    log "  Console: http://127.0.0.1:18011"
    log "  Login a WeChat account: $BIN_DIR/$BIN_NAME login"
}

main "$@"
