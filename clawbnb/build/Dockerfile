# Sandbox base image for weclawbot.
#
# One container per WeChat user per message. Spawned by the host weclawbot
# daemon via `podman run --runtime=runsc`. Ephemeral: --rm, --read-only root.
# Per-user state is mounted from the host sandbox dir.
#
# Two stages:
#   1. `mcp-builder` compiles `weclawbot-mcp`, the stdio JSON-RPC MCP server
#      Claude calls via the `attach` / `attach_url` tools. The host parses
#      stream-json `tool_use` events for those tools to know which generated
#      files to forward to the WeChat user.
#   2. The final image installs system packages, Python deps, the Claude CLI,
#      copies the MCP binary to /usr/local/bin/weclawbot-mcp, and creates the
#      runtime user.
#
# Critical layout decision: claude's binary stays where its installer puts
# it (/root/.local/share/claude/versions/<ver>), and we make /root world-
# readable so the non-root runtime user can exec it. A symlink at
# /usr/local/bin/claude is on PATH everywhere. That way weclawbot can
# bind-mount the entire per-user $HOME over /home/claude without hiding
# the binary.

# ---------- 1. weclawbot-mcp builder ----------
# Build context is `clawbnb/`. We only need the sibling `weclawbot-mcp/`
# package, not the main weclawbot binary — its dep graph is just serde +
# serde_json so this stage stays fast even under QEMU for arm64.
FROM rust:1-bookworm AS mcp-builder
WORKDIR /src
COPY weclawbot-mcp/Cargo.toml weclawbot-mcp/Cargo.lock* ./
COPY weclawbot-mcp/src ./src
# BuildKit cache mounts make subsequent rebuilds (especially in CI on cache
# miss for the COPY layers above) reuse the cargo registry + the
# /src/target dir — without these, every change to weclawbot-mcp/src/main.rs
# re-downloads serde from scratch. Requires `DOCKER_BUILDKIT=1` (default in
# modern docker / podman build).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release \
 && strip target/release/weclawbot-mcp \
 && cp target/release/weclawbot-mcp /weclawbot-mcp

# ---------- 2. Sandbox image ----------
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1

# 1. System packages
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        python3 \
        python3-pip \
        libreoffice-calc \
        poppler-utils \
        unzip \
 && rm -rf /var/lib/apt/lists/*

# 2. Python libs the official anthropics/skills document plugin uses
RUN pip3 install --break-system-packages --no-cache-dir \
        openpyxl \
        pandas \
        python-docx \
        python-pptx \
        pdfplumber

# 3. Claude CLI — installed as root, exposed to all users via PATH.
RUN curl -fsSL https://claude.ai/install.sh | bash \
 && chmod -R o+rX /root /root/.local /root/.claude 2>/dev/null \
 && chmod 755 /root \
 && ln -sf /root/.local/bin/claude /usr/local/bin/claude \
 && /usr/local/bin/claude --version

# 4. weclawbot-mcp — stdio JSON-RPC MCP server.
# Registered into each user's sandbox settings.json by the host
# (src/sandbox/materialize.rs) so Claude can call `attach` / `attach_url`.
COPY --from=mcp-builder /weclawbot-mcp /usr/local/bin/weclawbot-mcp
RUN chmod 755 /usr/local/bin/weclawbot-mcp

# 5. Runtime user. Strip the placeholder `ubuntu` (also UID 1000).
RUN userdel -r ubuntu 2>/dev/null || true \
 && useradd --create-home --home-dir /home/claude --shell /bin/bash --uid 1000 claude \
 && mkdir -p /home/claude/.claude /home/claude/media /work \
 && chown -R claude:claude /home/claude /work

USER claude:claude
WORKDIR /work

ENTRYPOINT ["claude"]
