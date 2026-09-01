#!/usr/bin/env bash
# Live integration test against a running herdr session.
#
# Drives the real thing: a pane running a fake login script, the real daemon
# talking to the real socket. Everything it writes lives in a temp dir, so the
# installed plugin's own config, state and once-ledger are untouched.
#
#   ./scripts/integration-test.sh
#
# Requires: a running herdr session (HERDR_SESSION or HERDR_SOCKET_PATH set),
# and a release build (cargo build --release).
set -euo pipefail

REPO=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BIN="$REPO/target/release/herdr-triggersd"
[ -x "$BIN" ] || { echo "build first: cargo build --release" >&2; exit 1; }

TMP=$(mktemp -d -t herdr-triggers-it-XXXXXX)
export HERDR_PLUGIN_CONFIG_DIR="$TMP/config"
export HERDR_PLUGIN_STATE_DIR="$TMP/state"
mkdir -p "$HERDR_PLUGIN_CONFIG_DIR" "$HERDR_PLUGIN_STATE_DIR"

# The daemon appends the herdr server's name to its state dir, so one machine
# can run a daemon per server without their pidfiles and locks colliding: the
# log is NOT directly under the state dir. Ask the binary where it writes rather
# than recomputing the rule here - duplicating it is how this went stale before.
DAEMON_STATE=$("$BIN" status 2>/dev/null | sed -n 's/^state dir: *//p' | head -1)
if [ -z "$DAEMON_STATE" ]; then
    printf 'cannot determine the daemon state dir from `%s status`\n' "$BIN" >&2
    exit 1
fi
DAEMON_LOG="$DAEMON_STATE/triggersd.log"

PANE=""
FAILURES=0

cleanup() {
    "$BIN" stop >/dev/null 2>&1 || true
    [ -n "$PANE" ] && herdr pane close "$PANE" >/dev/null 2>&1 || true
    rm -rf "$TMP"
}
trap cleanup EXIT

pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1"; FAILURES=$((FAILURES + 1)); }

check() {
    local label=$1 expected=$2
    if pane_text | grep -qF -- "$expected"; then pass "$label"; else fail "$label (missing: $expected)"; fi
}

check_absent() {
    local label=$1 unexpected=$2
    if pane_text | grep -qF -- "$unexpected"; then fail "$label (unexpected: $unexpected)"; else pass "$label"; fi
}

# The same source the rules match on. `recent-unwrapped` would still hold the
# previous attempt after a `clear`, so an "it did not happen again" assertion
# would read the first attempt's output and be meaningless.
pane_text() { herdr pane read "$PANE" --source visible --format text 2>/dev/null || true; }

# The daemon re-arms asynchronously. Sending the next login before it has
# finished would arm the rules against a screen that already shows the prompt,
# which is exactly the case the daemon refuses to fire on.
log_lines() { wc -l < "$DAEMON_LOG" 2>/dev/null || echo 0; }

# Only lines written after $from count: the log accumulates, so an identical
# line from earlier in the run would satisfy the wait immediately.
wait_for_log_after() {
    local from=$1 needle=$2 deadline=$((SECONDS + ${3:-15}))
    while [ "$SECONDS" -lt "$deadline" ]; do
        tail -n "+$((from + 1))" "$DAEMON_LOG" 2>/dev/null \
            | grep -qF -- "$needle" && return 0
        sleep 0.2
    done
    return 1
}

wait_for() {
    local needle=$1 deadline=$((SECONDS + ${2:-15}))
    while [ "$SECONDS" -lt "$deadline" ]; do
        pane_text | grep -qF -- "$needle" && return 0
        sleep 0.3
    done
    return 1
}

# A login that echoes what it was given, so the assertion is the pane's own output.
cat > "$TMP/fakelogin.sh" <<'EOS'
#!/bin/sh
printf 'TRIGGERLOGIN: '
read -r user
printf 'TRIGGERPASS: '
read -r pass
printf 'AUTHENTICATED user=%s pass=%s\n' "$user" "$pass"
EOS
chmod +x "$TMP/fakelogin.sh"

echo "creating probe pane"
PANE=$(herdr pane split --current --direction right --no-focus --cwd "$TMP" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')
herdr pane rename "$PANE" "triggers integration test" >/dev/null

cat > "$HERDR_PLUGIN_CONFIG_DIR/secrets.toml" <<'EOS'
TEST_USER = "admin"
TEST_PASS = "hunter2"
EOS
chmod 600 "$HERDR_PLUGIN_CONFIG_DIR/secrets.toml"

cat > "$HERDR_PLUGIN_CONFIG_DIR/triggers.toml" <<EOS
[settings]
poll_ms = 100
source = "visible"
fire_on_existing_text = true
cooldown_ms = 1000

[[rules]]
regex = "TRIGGERLOGIN:"
tail_within = 2
once = true
scope = { pane_id = "^${PANE}\$" }
action = { type = "send_text", text = "\${secret:TEST_USER}\n" }

[[rules]]
regex = "TRIGGERPASS:"
tail_within = 2
once = true
scope = { pane_id = "^${PANE}\$" }
action = { type = "send_text", text = "\${secret:TEST_PASS}\n" }

[[rules]]
regex = "AUTHENTICATED"
tail_within = 2
once = true
scope = { pane_id = "^${PANE}\$" }
action = { type = "tab_mark", marker = "OK " }
EOS

echo "starting daemon"
"$BIN" start
sleep 1
"$BIN" status | head -3

echo
echo "1. login automation"
herdr pane send-text "$PANE" "sh $TMP/fakelogin.sh"$'\n' >/dev/null
if wait_for "AUTHENTICATED" 20; then
    check "username was sent" "user=admin"
    check "password was sent" "pass=hunter2"
else
    fail "login never completed"
    pane_text | tail -5
fi

echo
echo "2. tab_mark action"
TAB=$(herdr pane get "$PANE" 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["tab_id"])' 2>/dev/null || echo "")
if [ -n "$TAB" ]; then
    LABEL=$(herdr tab list | python3 -c "
import json,sys
tabs = json.load(sys.stdin)['result']['tabs']
print(next((t['label'] for t in tabs if t['tab_id'] == '$TAB'), ''))")
    case "$LABEL" in
        # herdr decorates the label it reports with the tab number, so the
        # marker is not necessarily at the front.
        *OK*) pass "tab was marked (label: $LABEL)" ;;
        *) fail "tab was not marked (label: $LABEL)" ;;
    esac
else
    fail "could not resolve the pane's tab"
fi

echo
echo "3. once holds on a second login"
herdr pane send-text "$PANE" $'clear\n' >/dev/null
sleep 0.5
herdr pane send-text "$PANE" "sh $TMP/fakelogin.sh"$'\n' >/dev/null
sleep 6
# One pattern on one line: a multi-line argument would be an OR of two, and
# would pass whenever either half was absent.
check_absent "no second automatic login" "AUTHENTICATED user=admin"
if pane_text | grep -qF "TRIGGERLOGIN:"; then
    pass "prompt is waiting, unanswered"
else
    fail "prompt did not reappear"
fi
# Release the blocked script.
herdr pane send-text "$PANE" $'manual\nmanual\n' >/dev/null
sleep 1

echo
echo "4. ledger survives a daemon restart"
"$BIN" stop >/dev/null
sleep 1
"$BIN" start
sleep 1
FIRED=$("$BIN" status | grep -o '^[0-9]* once rules already fired' | grep -o '^[0-9]*' || echo 0)
if [ "${FIRED:-0}" -ge 3 ]; then
    pass "once-ledger survived the restart ($FIRED entries)"
else
    fail "once-ledger lost entries across restart ($FIRED)"
fi

echo
echo "5. reset re-arms"
BEFORE_RESET=$(log_lines)
"$BIN" reset >/dev/null
# Only the reset line: "watching N rule/pane pairs" is logged when the watched
# set CHANGES, and a reset changes neither the rule count nor the pane count, so
# waiting for it here could only ever time out.
if wait_for_log_after "$BEFORE_RESET" "reset: re-armed" 10; then
    pass "rules re-armed after reset"
else
    fail "daemon never re-armed after reset"
fi
sleep 0.5
# Clear and start the login as two sends with a gap between them: the daemon
# re-fires only once a poll has actually seen the match leave the screen tail,
# and it samples every poll_ms, so "clear; run" as one command line can move too
# fast to be observed.
herdr pane send-text "$PANE" $'clear\n' >/dev/null
sleep 0.5
herdr pane send-text "$PANE" "sh $TMP/fakelogin.sh"$'\n' >/dev/null
if wait_for "AUTHENTICATED" 20; then
    check "login ran again after reset" "user=admin"
else
    fail "reset did not re-arm the rules"
    pane_text | tail -5
fi

echo
echo "daemon log:"
sed 's/^/  /' "$DAEMON_LOG" 2>/dev/null | tail -20

echo
if [ "$FAILURES" -eq 0 ]; then
    echo "integration test: all checks passed"
else
    echo "integration test: $FAILURES check(s) failed"
fi
exit "$FAILURES"
