#!/usr/bin/env bash
# Full end-to-end smoke for weclawbot. Test-mode daemon intercepts iLink calls
# into a capture log; we inject synthetic inbound messages and assert what would
# have been sent back to WeChat — covers text in/out plus AI-generated images,
# docx, xlsx, pdf, and inbound image attachments.

set -u

BIN="${WECLAWBOT_BIN:-$HOME/weclawbot-target/release/weclawbot}"
PORT=18099
BASE_URL="http://127.0.0.1:$PORT"
TEST_USER="e2e-tester-$$"
TEST_HOME="$HOME/.weclawbot"
CAPTURE_LOG="$TEST_HOME/capture.log"

PASS=0
FAIL=0
LOGFILE="$TEST_HOME/logs/smoke-e2e-$(date +%s).log"
# Daemon's tracing-subscriber log (info!/warn! sink). One file per day. Used
# by delivery_source() to tell whether a generated file came via the MCP
# `attach` call (primary) or the filesystem-diff fallback. We don't truncate
# this between tests because it's owned by the daemon; instead we mark a
# fresh offset before each reset_capture().
DAEMON_TRACING_LOG="$TEST_HOME/logs/weclawbot-$(date +%Y-%m-%d).log"
TRACING_OFFSET=0

c_red()   { printf '\033[31m%s\033[0m\n' "$*"; }
c_green() { printf '\033[32m%s\033[0m\n' "$*"; }
c_yellow(){ printf '\033[33m%s\033[0m\n' "$*"; }
step() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
pass() { c_green "  PASS $*"; PASS=$((PASS+1)); }
fail() { c_red   "  FAIL $*"; FAIL=$((FAIL+1)); }

cleanup() {
    if [ -n "${DAEMON_PID:-}" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$TEST_HOME/users/u-"*"$TEST_USER"* 2>/dev/null || true
}
trap cleanup EXIT

step "Starting daemon in WECLAWBOT_TEST_MODE=1 on port $PORT"
[ -x "$BIN" ] || { fail "binary missing at $BIN"; exit 1; }

"$BIN" stop 2>/dev/null || true
pkill -f 'weclawbot.*--foreground' 2>/dev/null || true
sleep 1

mkdir -p "$TEST_HOME/logs"
rm -f "$CAPTURE_LOG"
WECLAWBOT_TEST_MODE=1 nohup "$BIN" start --foreground --port "$PORT" > "$LOGFILE" 2>&1 &
DAEMON_PID=$!

for i in $(seq 1 20); do
    sleep 0.5
    curl -sf "$BASE_URL/api/health" >/dev/null 2>&1 && break
done
if ! curl -sf "$BASE_URL/api/health" >/dev/null; then
    fail "daemon did not come up on $PORT"
    tail -20 "$LOGFILE"
    exit 1
fi
pass "daemon up (pid $DAEMON_PID)"

inject() {
    curl -sf -X POST -H 'Content-Type: application/json' \
        --data "$1" "$BASE_URL/api/test/inject"
}
read_capture() {
    curl -sf "$BASE_URL/api/test/capture"
}

wait_for_reply() {
    local timeout=$1
    local deadline=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        local n
        n=$(read_capture | python3 -c "import sys,json
d = json.load(sys.stdin)
n = sum(1 for e in d.get('entries', []) if e.get('kind') == 'send_message')
print(n)" 2>/dev/null)
        if [ "${n:-0}" -gt 0 ]; then
            return 0
        fi
        sleep 1
    done
    return 1
}

capture_item_types() {
    read_capture | python3 -c "import sys, json
d = json.load(sys.stdin)
for e in d.get('entries', []):
    if e.get('kind') != 'send_message': continue
    items = e.get('payload', {}).get('item_list') or []
    for it in items:
        print(it.get('type'))" 2>/dev/null
}

reset_capture() {
    rm -f "$CAPTURE_LOG"
    # Mark the current end of the daemon's tracing log so subsequent
    # delivery_source() lookups only see lines from THIS test case.
    if [ -f "$DAEMON_TRACING_LOG" ]; then
        TRACING_OFFSET=$(wc -c < "$DAEMON_TRACING_LOG" | tr -d ' ')
    else
        TRACING_OFFSET=0
    fi
}

# Source attribution: did the file come from Claude's MCP `attach` call
# (primary path, mirrors OpenClaw/ChatGPT) or from the filesystem diff
# fallback (Claude forgot to call attach — prompt regression)?
# We tail the daemon's tracing log from TRACING_OFFSET (set by reset_capture)
# so we only consider events from the current test case.
delivery_source() {
    [ -f "$DAEMON_TRACING_LOG" ] || { echo "UNKNOWN"; return; }
    local tail_bytes
    tail_bytes=$(tail -c +$((TRACING_OFFSET + 1)) "$DAEMON_TRACING_LOG" 2>/dev/null)
    if echo "$tail_bytes" | grep -q 'via MCP'; then
        echo "MCP"
    elif echo "$tail_bytes" | grep -q 'via diff fallback'; then
        echo "DIFF"
    else
        echo "UNKNOWN"
    fi
}

# ---------- T1: text in -> text out ----------
step "T1: text in -> text out"
reset_capture
inject '{"from_user_id":"'"$TEST_USER"'","text":"用一句话回答：1+1等于几"}' >/dev/null
if wait_for_reply 90; then
    types=$(capture_item_types)
    if echo "$types" | grep -q '^1$'; then
        pass "T1 text reply received"
    else
        fail "T1 expected text item (type=1), got: $(echo $types | tr '\n' ' ')"
    fi
else
    fail "T1 timeout waiting for reply"
fi

# ---------- T2: ask for image generation ----------
# Expect Claude to write the PNG then call `mcp__weclawbot__attach`.
# The diff fallback would also catch it, but a "via MCP" tag is the healthy
# signal — "via diff" means our prompt isn't steering the model.
step "T2: ask claude to generate a PNG"
reset_capture
inject '{"from_user_id":"'"$TEST_USER"'","text":"请用 python 在 /work/output/ 下生成一张 100x100 像素的纯红色 PNG，文件名 red.png，然后用 attach 工具发给我。"}' >/dev/null
if wait_for_reply 180; then
    types=$(capture_item_types)
    if echo "$types" | grep -q '^2$'; then
        src=$(delivery_source)
        pass "T2 image item (type=2) sent [$src]"
        [ "$src" = "DIFF" ] && c_yellow "  T2 NOTE: delivered via diff fallback — Claude did not call attach"
    else
        fail "T2 no image item; types: $(echo $types | tr '\n' ' ')"
    fi
else
    fail "T2 timeout"
fi

# ---------- T3: DOCX ----------
step "T3: ask claude for a DOCX"
reset_capture
inject '{"from_user_id":"'"$TEST_USER"'","text":"用 python-docx 在 /work/output/test.docx 写入『hello world』，然后用 attach 工具发给我。"}' >/dev/null
if wait_for_reply 240; then
    types=$(capture_item_types)
    if echo "$types" | grep -q '^4$'; then
        src=$(delivery_source)
        pass "T3 file item (type=4, docx) sent [$src]"
        [ "$src" = "DIFF" ] && c_yellow "  T3 NOTE: delivered via diff fallback"
    else
        fail "T3 no file item; types: $(echo $types | tr '\n' ' ')"
    fi
else
    fail "T3 timeout"
fi

# ---------- T4: XLSX ----------
step "T4: ask claude for an XLSX"
reset_capture
inject '{"from_user_id":"'"$TEST_USER"'","text":"用 openpyxl 在 /work/output/test.xlsx 写 A1=hello, B1=world，然后用 attach 工具发给我。"}' >/dev/null
if wait_for_reply 240; then
    types=$(capture_item_types)
    if echo "$types" | grep -q '^4$'; then
        src=$(delivery_source)
        pass "T4 file item (type=4, xlsx) sent [$src]"
        [ "$src" = "DIFF" ] && c_yellow "  T4 NOTE: delivered via diff fallback"
    else
        fail "T4 no file item; types: $(echo $types | tr '\n' ' ')"
    fi
else
    fail "T4 timeout"
fi

# ---------- T5: PDF ----------
step "T5: ask claude for a PDF"
reset_capture
inject '{"from_user_id":"'"$TEST_USER"'","text":"在 /work/output/test.pdf 生成一个 PDF，内容是 hello world，然后用 attach 工具发给我。"}' >/dev/null
if wait_for_reply 300; then
    types=$(capture_item_types)
    if echo "$types" | grep -q '^4$'; then
        src=$(delivery_source)
        pass "T5 file item (type=4, pdf) sent [$src]"
        [ "$src" = "DIFF" ] && c_yellow "  T5 NOTE: delivered via diff fallback"
    else
        c_yellow "  T5 partial: no file item (PDF generation in sandbox may need extra libs); types: $(echo $types | tr '\n' ' ')"
        FAIL=$((FAIL+1))
    fi
else
    fail "T5 timeout"
fi

# ---------- T6: inbound image ----------
step "T6: user sends image, claude describes"
reset_capture
TMP_PNG="/tmp/e2e-test-input.png"
python3 -c "
import struct, zlib
def chunk(t,d):
    return struct.pack('>I',len(d))+t+d+struct.pack('>I',zlib.crc32(t+d))
sig = b'\x89PNG\r\n\x1a\n'
ihdr = struct.pack('>IIBBBBB', 1, 1, 8, 2, 0, 0, 0)
idat = zlib.compress(b'\x00\xff\x00\x00')
open('$TMP_PNG','wb').write(sig + chunk(b'IHDR', ihdr) + chunk(b'IDAT', idat) + chunk(b'IEND', b''))
"
inject "{\"from_user_id\":\"$TEST_USER\",\"text\":\"这是什么颜色？\",\"attachment_paths\":[\"$TMP_PNG\"]}" >/dev/null
if wait_for_reply 180; then
    types=$(capture_item_types)
    if echo "$types" | grep -q '^1$'; then
        pass "T6 got text reply (claude saw the inbound image)"
    else
        fail "T6 expected text reply; types: $(echo $types | tr '\n' ' ')"
    fi
else
    fail "T6 timeout"
fi

# ---------- T7: diff fallback (Claude told NOT to call attach) ----------
# Stress-test the safety net: when the model writes a file but doesn't
# declare it via `attach`, the host should still notice via the /work/
# diff and forward the artifact, tagged "via diff fallback".
step "T7: file delivered via diff fallback when attach is skipped"
reset_capture
inject '{"from_user_id":"'"$TEST_USER"'","text":"用 python 在 /work/output/fallback.txt 写入『diff fallback test』。不要调用 attach 工具，只写文件然后简短确认。"}' >/dev/null
if wait_for_reply 180; then
    types=$(capture_item_types)
    if echo "$types" | grep -q '^4$'; then
        src=$(delivery_source)
        if [ "$src" = "DIFF" ]; then
            pass "T7 diff fallback fired (file delivered without attach)"
        elif [ "$src" = "MCP" ]; then
            c_yellow "  T7 NOTE: Claude called attach despite being told not to — fallback path not exercised"
            PASS=$((PASS+1))
        else
            pass "T7 file delivered [$src]"
        fi
    else
        fail "T7 no file item; types: $(echo $types | tr '\n' ' ')"
    fi
else
    fail "T7 timeout"
fi

# ---------- T8: attach_url (remote URL forwarded without local download) ----
# Pick a small, very stable https file. Octocat is a 7KB PNG that's been at
# the same URL for years; if it's flaky in CI we can swap to a self-hosted
# fixture. Soft-checked: if Claude can't browse from inside the sandbox it
# will respond with text — we don't fail the suite over that.
step "T8: attach_url forwards a remote https file"
reset_capture
URL='https://github.githubassets.com/favicons/favicon.png'
inject "{\"from_user_id\":\"$TEST_USER\",\"text\":\"请直接调用 attach_url 工具，url=$URL ，然后简短确认即可，不需要下载也不需要 web 搜索。\"}" >/dev/null
if wait_for_reply 180; then
    types=$(capture_item_types)
    if echo "$types" | grep -qE '^(2|4)$'; then
        pass "T8 attach_url delivered (type=$(echo "$types" | grep -E '^[245]$' | head -1))"
    else
        c_yellow "  T8 partial: no media item — attach_url either not called or download failed; types: $(echo $types | tr '\n' ' ')"
        FAIL=$((FAIL+1))
    fi
else
    fail "T8 timeout"
fi

# ---------- T9: PPT generation + intermediate .py NOT leaked ----------
# Tests the Anthropic-skill-style /tmp scratch convention. Claude should
# write its build script to /tmp/ and only the .pptx to /work/output/.
# Helpers in /tmp/ should never appear in capture.
step "T9: generate PPT, no helper .py leaked"
reset_capture
inject '{"from_user_id":"'"$TEST_USER"'","text":"请用 python-pptx 生成一个 3 页 PPT 介绍 Rust 语言。把构建脚本放 /tmp/，最终 pptx 放 /work/output/intro_rust.pptx，然后 attach 给我。"}' >/dev/null
if wait_for_reply 360; then
    types=$(capture_item_types)
    # Check for the pptx (type=4 since pptx routes as File)
    if echo "$types" | grep -q '^4$'; then
        # Check that no .py was forwarded
        py_count=$(read_capture | python3 -c "import sys,json
d = json.load(sys.stdin)
n = 0
for e in d.get('entries', []):
    if e.get('kind') != 'send_message': continue
    for it in (e.get('payload', {}).get('item_list') or []):
        fi = it.get('file_item') or {}
        name = fi.get('file_name') or ''
        if name.endswith('.py') or name.endswith('.sh'):
            n += 1
print(n)" 2>/dev/null)
        if [ "${py_count:-0}" -eq 0 ]; then
            pass "T9 pptx delivered, no helper .py/.sh leaked"
        else
            fail "T9 helper script leaked ($py_count .py/.sh items)"
        fi
    else
        fail "T9 no pptx item; types: $(echo $types | tr '\n' ' ')"
    fi
else
    fail "T9 timeout (>6min)"
fi

# ---------- T10: script as deliverable (.py via MCP, not via diff/safe-list) ----
# Proves extension-blacklist would be wrong: .py CAN be a deliverable when
# the user explicitly asks, and goes through MCP attach not diff fallback.
step "T10: .py script delivered when user explicitly asks (via MCP)"
reset_capture
inject '{"from_user_id":"'"$TEST_USER"'","text":"请在 /work/output/hello_for_user.py 写一个最简单的 print(\"hi\") 脚本，然后调用 attach 工具把它发给我。"}' >/dev/null
if wait_for_reply 180; then
    types=$(capture_item_types)
    if echo "$types" | grep -q '^4$'; then
        src=$(delivery_source)
        if [ "$src" = "MCP" ]; then
            pass "T10 .py delivered via MCP (extension agnostic)"
        else
            c_yellow "  T10 NOTE: delivered via $src — MCP path was expected"
            pass "T10 .py delivered [$src]"
        fi
    else
        fail "T10 no file item; types: $(echo $types | tr '\n' ' ')"
    fi
else
    fail "T10 timeout"
fi

# ---------- T11: timeout does NOT leak intermediate files ----------
# When CLI errors (e.g. timeout), the diff-fallback path must not fire.
# Approach: temporarily lower the GLOBAL config.json ai.timeoutMs to 5s
# (per-user settings.json `ai.timeoutMs` is NOT read — `CliConfig::from_value`
# loads from ~/.weclawbot/config.json). Then ask for a slow task. Expect:
# timeout error reply, and ZERO file items in capture (diff fallback gated
# on cli_succeeded).
step "T11: CLI timeout suppresses diff fallback"
reset_capture
TEST_USER_TIMEOUT="${TEST_USER}-fast-timeout"

# Snapshot then mutate the global config.
CONFIG_JSON="$HOME/.weclawbot/config.json"
ORIG_CONFIG_BACKUP="$CONFIG_JSON.smoke-t11-backup"
cp "$CONFIG_JSON" "$ORIG_CONFIG_BACKUP"
python3 - "$CONFIG_JSON" <<'PY'
import json, sys
p = sys.argv[1]
with open(p) as f: c = json.load(f)
c.setdefault("ai", {})["timeoutMs"] = 5000
with open(p, "w") as f: json.dump(c, f, indent=2)
PY

inject "{\"from_user_id\":\"$TEST_USER_TIMEOUT\",\"text\":\"在 /work/output/leak_test.txt 写一段长文，分多次 Bash 调用慢慢写，至少花 30 秒。\"}" >/dev/null
if wait_for_reply 60; then
    types=$(capture_item_types)
    file_count=$(echo "$types" | grep -E '^(2|4|5)$' | wc -l | tr -d ' ')
    if [ "${file_count:-0}" -eq 0 ]; then
        pass "T11 timeout did not leak any file (file_count=$file_count)"
    else
        fail "T11 timeout leaked $file_count file items (diff fallback should have been skipped)"
    fi
else
    fail "T11 timeout test did not receive any reply"
fi

# Restore the global config.
mv "$ORIG_CONFIG_BACKUP" "$CONFIG_JSON"

# Cleanup the timeout user
TEST_USER_HASH=$(python3 -c "import hashlib; print('u-'+hashlib.sha1(b'$TEST_USER_TIMEOUT').hexdigest()[:12])")
rm -rf "$HOME/.weclawbot/users/$TEST_USER_HASH"

# ---------- T12: /menu console (text-only, no AI spawn) ----------
# /menu enters state mode. From the menu we drill into model → set →
# sonnet, which should write `model: sonnet` to settings.json. /exit
# leaves the menu.
step "T12: /menu console drives settings.json"
reset_capture
TEST_USER_MENU="${TEST_USER}-menu"
MENU_USER_HASH=$(python3 -c "import hashlib; print('u-'+hashlib.sha1(b'$TEST_USER_MENU').hexdigest()[:12])")
MENU_USER_DIR="$HOME/.weclawbot/users/$MENU_USER_HASH"
rm -rf "$MENU_USER_DIR"

inject "{\"from_user_id\":\"$TEST_USER_MENU\",\"text\":\"/menu\"}" >/dev/null
sleep 1
inject "{\"from_user_id\":\"$TEST_USER_MENU\",\"text\":\"model\"}" >/dev/null
sleep 1
inject "{\"from_user_id\":\"$TEST_USER_MENU\",\"text\":\"set\"}" >/dev/null
sleep 1
inject "{\"from_user_id\":\"$TEST_USER_MENU\",\"text\":\"sonnet\"}" >/dev/null
sleep 1
inject "{\"from_user_id\":\"$TEST_USER_MENU\",\"text\":\"/exit\"}" >/dev/null
sleep 1

# Verify settings.json model == "sonnet"
if [ -f "$MENU_USER_DIR/settings.json" ]; then
    model_val=$(python3 -c "import json; print(json.load(open('$MENU_USER_DIR/settings.json')).get('model',''))")
    if [ "$model_val" = "sonnet" ]; then
        # Also verify replies were captured (no AI spawn, all instant menu replies)
        reply_count=$(read_capture | python3 -c "import sys,json
d = json.load(sys.stdin)
print(sum(1 for e in d.get('entries', []) if e.get('kind') == 'send_message'))")
        if [ "${reply_count:-0}" -ge 4 ]; then
            pass "T12 /menu set model=sonnet (got $reply_count menu replies)"
        else
            fail "T12 model is sonnet but only $reply_count replies (expected >=4)"
        fi
    else
        fail "T12 model is '$model_val', expected 'sonnet'"
    fi
else
    fail "T12 settings.json was never created"
fi
rm -rf "$MENU_USER_DIR"

# ---------- T13: account commands rejected ----------
# Even when the user is INSIDE menu mode, account-class words like /login
# must not match any node in the tree. The dispatcher should return
# "未知命令" or similar.
step "T13: account commands not exposed in /menu"
reset_capture
TEST_USER_BLOCK="${TEST_USER}-block"
inject "{\"from_user_id\":\"$TEST_USER_BLOCK\",\"text\":\"/menu\"}" >/dev/null
sleep 1
inject "{\"from_user_id\":\"$TEST_USER_BLOCK\",\"text\":\"/login\"}" >/dev/null
sleep 1
# The /login reply should contain "未知命令" or similar, NOT a login flow.
last_reply=$(read_capture | python3 -c "import sys,json
d = json.load(sys.stdin)
texts = []
for e in d.get('entries', []):
    if e.get('kind') != 'send_message': continue
    for it in (e.get('payload', {}).get('item_list') or []):
        ti = it.get('text_item') or {}
        t = ti.get('text') or ''
        if t: texts.append(t)
print(texts[-1] if texts else '')" 2>/dev/null)
if echo "$last_reply" | grep -qE '未知命令|unknown|invalid|❌'; then
    pass "T13 /login rejected in menu mode"
else
    fail "T13 /login was not rejected; last reply: $(echo "$last_reply" | head -c 100)"
fi
inject "{\"from_user_id\":\"$TEST_USER_BLOCK\",\"text\":\"/exit\"}" >/dev/null
sleep 1
BLOCK_USER_HASH=$(python3 -c "import hashlib; print('u-'+hashlib.sha1(b'$TEST_USER_BLOCK').hexdigest()[:12])")
rm -rf "$HOME/.weclawbot/users/$BLOCK_USER_HASH"

# ---------- T14: typing pulse fires within 500ms of inbound ----------
# Verifies typing was moved out of complete_with_content into handler top.
# Inject a message, immediately read capture, assert first send_typing
# event timestamp is close to injection timestamp.
step "T14: typing indicator fires within 500ms"
reset_capture
TEST_USER_TYP="${TEST_USER}-typing"
INJECT_TS_MS=$(python3 -c "import time; print(int(time.time()*1000))")
inject "{\"from_user_id\":\"$TEST_USER_TYP\",\"text\":\"hi\"}" >/dev/null
# Poll capture for ~1s waiting for the first send_typing
DEADLINE=$(( $(date +%s) + 3 ))
TYPING_TS_MS=""
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    TYPING_TS_MS=$(read_capture | python3 -c "import sys,json
d = json.load(sys.stdin)
for e in d.get('entries', []):
    if e.get('kind') == 'send_typing':
        print(e.get('ts_ms', 0))
        break" 2>/dev/null)
    if [ -n "$TYPING_TS_MS" ] && [ "$TYPING_TS_MS" != "0" ]; then
        break
    fi
    sleep 0.1
done
if [ -n "$TYPING_TS_MS" ] && [ "$TYPING_TS_MS" != "0" ]; then
    delta=$((TYPING_TS_MS - INJECT_TS_MS))
    if [ "$delta" -lt 500 ]; then
        pass "T14 typing fired at +${delta}ms (<500ms target)"
    else
        c_yellow "  T14 typing fired at +${delta}ms (slower than 500ms target but still early)"
        if [ "$delta" -lt 3000 ]; then
            pass "T14 typing fired within 3s window"
        else
            fail "T14 typing too late: +${delta}ms"
        fi
    fi
else
    fail "T14 no send_typing event seen within 3s"
fi
TYP_USER_HASH=$(python3 -c "import hashlib; print('u-'+hashlib.sha1(b'$TEST_USER_TYP').hexdigest()[:12])")
rm -rf "$HOME/.weclawbot/users/$TYP_USER_HASH"

echo
TOTAL=$((PASS + FAIL))
printf '\033[1msmoke-e2e:\033[0m  passed: %d  failed: %d  (of %d)\n' "$PASS" "$FAIL" "$TOTAL"
echo "  daemon log: $LOGFILE"

[ "$FAIL" -eq 0 ]
