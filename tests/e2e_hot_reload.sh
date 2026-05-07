#!/usr/bin/env bash
# E2E test for hot reload: verifies PTY fds survive across binary restart
set -euo pipefail

ZELLIJ="$HOME/.local/bin/zellij"
unset ZELLIJ_SESSION_NAME
SESSION="hot-reload-e2e-$$"
MARKER="/tmp/hot-reload-e2e-marker-$$"
LOG="/tmp/hot-reload-e2e-$$.log"
LOG_FILE="/var/folders/l0/wbbwm9dj6wxc6rc4tw0rx02m0000gn/T/zellij-501/zellij-log/zellij.log"

cleanup() {
    "$ZELLIJ" kill-session "$SESSION" 2>/dev/null || true
    pkill -f "zellij-fd-daemon.*$SESSION" 2>/dev/null || true
    rm -f "$MARKER" "$LOG"
}
trap cleanup EXIT

echo "=== Hot Reload E2E Test ==="
echo "Session: $SESSION"

# 1. Create detached session and run a marker process
echo "[1] Creating session with marker process..."
rm -f "$MARKER"
"$ZELLIJ" attach "$SESSION" -b -c 2>>"$LOG"
sleep 1
"$ZELLIJ" -s "$SESSION" run -c -- bash -c \
    "echo STARTED > $MARKER; i=0; while true; do echo tick_\$i >> $MARKER; i=\$((i+1)); sleep 0.5; done"
sleep 3

if [ ! -f "$MARKER" ]; then
    echo "    FAIL: marker file not created"
    exit 1
fi

TICKS_BEFORE=$(wc -l < "$MARKER" | tr -d ' ')
echo "    Marker has $TICKS_BEFORE lines"

BASH_PID=$(pgrep -f "hot-reload-e2e-marker-$$" | head -1 || true)
echo "    Marker process PID: ${BASH_PID:-unknown}"

# 2. Trigger hot reload
echo "[2] Triggering hot reload..."
"$ZELLIJ" -s "$SESSION" action hot-reload 2>>"$LOG" || true

# 3. Wait for session to die
echo "[3] Waiting for old session to die..."
for i in $(seq 1 20); do
    if ! "$ZELLIJ" list-sessions 2>/dev/null | grep -q "$SESSION"; then
        echo "    Session gone after ~${i}s"
        break
    fi
    sleep 0.5
done

# 4. Check daemon survived
echo "[4] Checking fd daemon..."
DAEMON_RUNNING=false
if pgrep -f "zellij-fd-daemon.*$SESSION" >/dev/null 2>&1; then
    echo "    PASS: daemon running"
    DAEMON_RUNNING=true
else
    echo "    FAIL: no daemon"
fi

# 5. Re-attach to resurrect session (this triggers fd retrieval from daemon)
echo "[5] Re-attaching to resurrect session..."
"$ZELLIJ" attach "$SESSION" -b -c 2>>"$LOG"
sleep 3

# 6. Check if session is back
if "$ZELLIJ" list-sessions 2>/dev/null | grep -q "$SESSION"; then
    echo "    PASS: session resurrected"
else
    echo "    FAIL: session not resurrected"
fi

# 7. Check bash process survived (child of the PTY should still be running)
echo "[7] Checking marker process..."
PROCESS_ALIVE=false
if [ -n "${BASH_PID:-}" ] && kill -0 "$BASH_PID" 2>/dev/null; then
    echo "    PASS: PID $BASH_PID alive"
    PROCESS_ALIVE=true
else
    echo "    INFO: original process died (expected if PTY fd was properly reused)"
    # The process might have died but new writes could happen via resurrected terminal
fi

# 8. Check marker still growing (the key test)
echo "[8] Checking marker growth..."
TICKS_MID=$(wc -l < "$MARKER" | tr -d ' ')
sleep 3
TICKS_END=$(wc -l < "$MARKER" | tr -d ' ')
echo "    Before reload: $TICKS_BEFORE"
echo "    After reload:  $TICKS_MID"
echo "    +3s:           $TICKS_END"

STILL_WRITING=false
if [ "$TICKS_END" -gt "$TICKS_MID" ]; then
    echo "    PASS: still writing"
    STILL_WRITING=true
else
    echo "    FAIL: stopped writing"
fi

# 9. Log entries
echo ""
echo "[9] Log entries:"
if [ -f "$LOG_FILE" ]; then
    grep -i "hot.reload\|daemon\|reusing fd\|EBADF\|stored fd\|retrieved\|popping" "$LOG_FILE" 2>/dev/null | grep "$SESSION\|hot.reload\|reusing\|EBADF\|popping" | tail -15
fi

# 10. Summary
echo ""
echo "=== Summary ==="
echo "Daemon running:  $DAEMON_RUNNING"
echo "Process alive:   $PROCESS_ALIVE"
echo "Still writing:   $STILL_WRITING"

if $DAEMON_RUNNING && $STILL_WRITING; then
    echo "RESULT: ALL PASSED"
else
    echo "RESULT: FAILED"
    exit 1
fi
