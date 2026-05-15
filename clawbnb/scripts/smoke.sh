#!/usr/bin/env bash
# weclawbot smoke test — runs after `cargo build --release` to catch
# regressions in the daemon/sandbox/claude pipeline BEFORE the user sees them.
#
# Prerequisites (all should already be set up by install.sh):
#   - podman + runsc registered + functional
#   - sandbox base image present (localhost/weclawbot-sandbox-base:dev or ghcr.io/...)
#   - ~/.claude/.credentials.json exists (operator logged into claude)
#
# Run:
#   scripts/smoke.sh                  # full pipeline
#   scripts/smoke.sh --quick          # skip the live claude reply test
#
# Designed to be cheap (~10s when --quick, ~20s full) and produce clear FAIL
# lines that map 1:1 to the bug that broke things.

set -u
QUICK=0
[ "${1:-}" = "--quick" ] && QUICK=1

PROJ_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${WECLAWBOT_BIN:-$HOME/weclawbot-target/release/weclawbot}"
[ -x "$BIN" ] || BIN="$PROJ_ROOT/target/release/weclawbot"

PASS=0
FAIL=0
SKIP=0

c_red()   { printf '\033[31m%s\033[0m\n' "$*"; }
c_green() { printf '\033[32m%s\033[0m\n' "$*"; }
c_yellow(){ printf '\033[33m%s\033[0m\n' "$*"; }

pass() { c_green "  PASS $*"; PASS=$((PASS+1)); }
fail() { c_red   "  FAIL $*"; FAIL=$((FAIL+1)); }
skip() { c_yellow "  SKIP $*"; SKIP=$((SKIP+1)); }

step() { printf '\n\033[1;36m==>\033[0m %s\n' "$*"; }

# ---------- 1. Binary exists & runs ----------
step "binary"
if [ ! -x "$BIN" ]; then
    fail "binary not found at $BIN"
    exit 1
fi
"$BIN" version >/dev/null 2>&1 && pass "version" || fail "version"

# ---------- 2. doctor passes ----------
step "doctor"
DOC_OUT=$("$BIN" doctor 2>&1)
if echo "$DOC_OUT" | grep -q "Status: READY"; then
    pass "doctor READY"
else
    fail "doctor not READY:"
    echo "$DOC_OUT" | tail -20
fi

# ---------- 3. defaults file exists & is valid JSON ----------
step "defaults"
DEFAULTS_PATH="$HOME/.weclawbot/defaults/claude-settings.json"
if [ -f "$DEFAULTS_PATH" ] && python3 -c "import json; json.load(open(\"$DEFAULTS_PATH\"))" 2>/dev/null; then
    pass "defaults JSON valid"
else
    fail "defaults missing or invalid"
fi

# ---------- 4. podman + runsc smoke (re-run independently of doctor) ----------
step "podman+runsc"
if podman run --rm --runtime=runsc docker.io/library/alpine:3 sh -c 'uname -r' 2>/dev/null | grep -q gvisor; then
    pass "gVisor kernel intercept"
else
    fail "podman+runsc smoke (gvisor kernel not seen)"
fi

# ---------- 5. sandbox image present ----------
step "sandbox image"
IMG="$($BIN config show 2>/dev/null | python3 -c 'import sys,json,re;
out=sys.stdin.read()
# crude: find "image": "<value>"
m=re.search(r"\"image\"\s*:\s*\"([^\"]+)\"", out)
print(m.group(1) if m else "")' 2>/dev/null)"
IMG="${IMG:-localhost/weclawbot-sandbox-base:dev}"
if podman image exists "$IMG"; then
    pass "$IMG cached"
else
    fail "$IMG not present"
fi

# ---------- 6. PIPELINE: bind-mounted credentials are READABLE inside container ----------
# This is the bug we just hit (uid remapping made credentials.json owned by
# root inside the container, unreadable to the non-root claude user).
step "credentials readable in container"
if podman run --rm --userns=keep-id:uid=1000,gid=1000 \
    -v "$HOME/.claude/.credentials.json:/home/claude/.claude/.credentials.json:ro" \
    --entrypoint=cat \
    "$IMG" \
    /home/claude/.claude/.credentials.json >/dev/null 2>&1
then
    pass "credentials.json readable as in-container user 1000"
else
    fail "credentials.json NOT readable in container — check --userns flag and file perms"
fi

# ---------- 7. PIPELINE: full claude reply round-trip ----------
if [ "$QUICK" = "1" ]; then
    step "live claude reply"
    skip "(--quick)"
else
    step "live claude reply"
    # Use a fresh test sandbox so we don't perturb real users.
    TEST_HASH="u-smoketest$$"
    TEST_SANDBOX="$HOME/.weclawbot/users/$TEST_HASH/sandbox"
    mkdir -p "$TEST_SANDBOX/home/.claude/plugins" \
             "$TEST_SANDBOX/home/.claude/projects" \
             "$TEST_SANDBOX/work" \
             "$TEST_SANDBOX/media/inbound"
    cp "$DEFAULTS_PATH" "$TEST_SANDBOX/home/.claude/settings.json" 2>/dev/null \
        || echo '{}' > "$TEST_SANDBOX/home/.claude/settings.json"

    REPLY=$(echo "ping" | podman run --rm -i --runtime=runsc \
        --userns=keep-id:uid=1000,gid=1000 \
        --network=bridge \
        -e HOME=/home/claude \
        -v "$TEST_SANDBOX/home/.claude:/home/claude/.claude:rw" \
        -v "$TEST_SANDBOX/work:/work:rw" \
        -v "$HOME/.claude/.credentials.json:/home/claude/.claude/.credentials.json:ro" \
        --workdir=/work \
        "$IMG" \
        -p --output-format text --dangerously-skip-permissions 2>&1)
    EXIT=$?
    rm -rf "$HOME/.weclawbot/users/$TEST_HASH"

    if [ $EXIT -eq 0 ] && [ -n "$REPLY" ] && ! echo "$REPLY" | grep -qi "Error\|not logged in\|EROFS\|EACCES"; then
        pass "claude replied: $(printf '%s' "$REPLY" | head -c 60)…"
    else
        fail "claude failed (exit=$EXIT): $REPLY"
    fi
fi

# ---------- 8. PIPELINE: claude can write home-level state files (~/.claude.json) ----------
# This caught a bug where --read-only blocked claude from writing its own
# session cache (`/home/claude/.claude.json`), causing exit 1 with EROFS.
step "container HOME is writable"
TEST_SANDBOX_W="$HOME/.weclawbot/users/u-smoke-wri$$/sandbox"
mkdir -p "$TEST_SANDBOX_W/home/.claude" "$TEST_SANDBOX_W/work" "$TEST_SANDBOX_W/media/inbound"
if podman run --rm --userns=keep-id:uid=1000,gid=1000 \
    --read-only --tmpfs /tmp --tmpfs /run \
    -v "$TEST_SANDBOX_W/home:/home/claude:rw" \
    --entrypoint=sh "$IMG" \
    -c 'echo "hi" > /home/claude/.claude.json && cat /home/claude/.claude.json' >/dev/null 2>&1
then
    pass "writes to /home/claude/.claude.json succeed"
else
    fail "cannot write to /home/claude/.claude.json (would break claude state)"
fi
rm -rf "$(dirname "$TEST_SANDBOX_W")"

# ---------- 9. daemon end-to-end via running weclawbot ----------
# If a daemon is currently running, hit /api/health and verify it can spawn
# claude through the same code path the WeChat handler would use.
step "running daemon end-to-end"
if curl -sf http://127.0.0.1:18011/api/health >/dev/null 2>&1; then
    # Pick (or create) a test user, write a settings file, then trigger a
    # tiny synthetic interaction via the API once we have one.
    # For now: verify the daemon's spawn args match smoke args.
    DAEMON_PID=$(pgrep -f 'weclawbot.*--foreground' | head -1)
    if [ -n "$DAEMON_PID" ]; then
        ARGS=$(tr '\0' ' ' < /proc/$DAEMON_PID/cmdline 2>/dev/null)
        pass "daemon running ($DAEMON_PID): $(printf '%s' "$ARGS" | head -c 80)"
    else
        skip "no foreground daemon process"
    fi
else
    skip "no daemon on :18011 (run `weclawbot start`)"
fi

# ---------- summary ----------
echo
TOTAL=$((PASS + FAIL + SKIP))
printf '\033[1m%s\033[0m  passed: %d  failed: %d  skipped: %d  (of %d)\n' \
    "smoke results:" "$PASS" "$FAIL" "$SKIP" "$TOTAL"

[ "$FAIL" -eq 0 ]
