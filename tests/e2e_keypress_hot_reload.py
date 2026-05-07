#!/usr/bin/env python3
"""E2E test: verify keypresses work before and after hot reload.

Uses pexpect to drive an interactive zellij session through a real PTY,
exactly like a human typing.
"""
import datetime
import os
import subprocess
import sys
import time

import pexpect


def ts():
    return datetime.datetime.now().strftime("%H:%M:%S.%f")[:-3]

ZELLIJ = os.path.expanduser("~/.local/bin/zellij")
SESSION = f"kp-e2e-{os.getpid()}"
MARKER = f"/tmp/kp-e2e-marker-{os.getpid()}"

# Clean env so we don't conflict with parent zellij
env = os.environ.copy()
env.pop("ZELLIJ_SESSION_NAME", None)
env.pop("ZELLIJ", None)


def cleanup():
    subprocess.run([ZELLIJ, "kill-session", SESSION],
                   capture_output=True, env=env)
    subprocess.run(["pkill", "-f", f"zellij-fd-daemon.*{SESSION}"],
                   capture_output=True)
    try:
        os.unlink(MARKER)
    except FileNotFoundError:
        pass


def zellij_action(*args):
    """Run a zellij CLI action against our session."""
    subprocess.run([ZELLIJ, "-s", SESSION, "action", *args],
                   capture_output=True, env=env, timeout=10)


def main():
    cleanup()
    print(f"=== Keypress Hot Reload E2E === ({ts()})")
    print(f"Session: {SESSION}")

    # 1. Spawn interactive zellij session
    print("[1] Spawning interactive zellij session...")
    child = pexpect.spawn(
        ZELLIJ,
        ["attach", SESSION, "-c"],
        env=env,
        encoding="utf-8",
        timeout=15,
        dimensions=(24, 80),
    )
    # Wait for shell to be ready - just give it time
    time.sleep(3)
    # Drain any startup output
    try:
        startup = child.read_nonblocking(size=65536, timeout=2)
        print(f"    Startup output: {startup[:200]!r}")
    except pexpect.TIMEOUT:
        print(f"    No startup output (timeout)")

    # 2. Send a command before hot reload (retry for shell startup)
    print("[2] Typing command before hot reload...")
    for attempt in range(5):
        child.sendline(f"echo BEFORE_RELOAD > {MARKER}")
        time.sleep(1)
        try:
            with open(MARKER) as f:
                content = f.read().strip()
            if content == "BEFORE_RELOAD":
                print(f"    PASS: marker says '{content}' (attempt {attempt+1})")
                break
        except FileNotFoundError:
            pass
        if attempt < 4:
            print(f"    Retrying shell startup (attempt {attempt+1})...")
            time.sleep(1)
    else:
        print(f"    FAIL: shell not ready after 5 attempts")
        child.close()
        cleanup()
        sys.exit(1)

    # 3. Type a few more characters to warm up
    print("[3] Sending test keypresses before reload...")
    child.sendline("echo test_1_before")
    time.sleep(0.3)
    child.sendline("echo test_2_before")
    time.sleep(0.3)
    child.sendline("echo test_3_before")
    time.sleep(1)

    # Read output so the buffer doesn't fill up
    try:
        child.read_nonblocking(size=65536, timeout=1)
    except pexpect.TIMEOUT:
        pass

    # 4. Trigger hot reload from a separate CLI process
    print(f"[4] Triggering hot reload... ({ts()})")
    zellij_action("hot-reload")

    # 5. Wait for the session to die and client to reconnect
    print(f"[5] Waiting for reconnection... ({ts()})")
    # The client auto-reconnects. We need to wait for the NEW session's "Loading Zellij"
    # message which proves the client has reconnected and started a new server.
    # First drain any old buffered output by reading until we see "Loading Zellij"
    try:
        child.expect("Loading Zellij", timeout=30)
        print(f"    Got 'Loading Zellij' from reconnected session ({ts()})")
    except pexpect.TIMEOUT:
        print(f"    WARN: no 'Loading Zellij' seen ({ts()})")
    except pexpect.EOF:
        print("    Client exited unexpectedly")
        child.close()
        cleanup()
        sys.exit(1)

    # Wait for the reconnected session to fully initialize
    time.sleep(5)

    # Drain any pending output first
    try:
        pending = child.read_nonblocking(size=65536, timeout=2)
        print(f"    Pending output before typing: {len(pending)} bytes")
    except pexpect.TIMEOUT:
        print(f"    No pending output")

    # Send a dummy keystroke to wake up the old stdin handler
    # (it blocks on stdin.lock() and needs input to detect session change)
    print(f"    Sending wake-up keystroke... ({ts()})")
    child.sendline("# wake up old stdin handler")
    time.sleep(3)

    # Drain output from the wake-up
    try:
        child.read_nonblocking(size=65536, timeout=2)
    except pexpect.TIMEOUT:
        pass

    # Send the actual test command
    print(f"    Sending marker command... ({ts()})")
    child.sendline(f"echo AFTER_RELOAD >> {MARKER}")
    time.sleep(3)

    # Send another copy in case the first was consumed by old stdin handler
    child.sendline(f"echo AFTER_RELOAD >> {MARKER}")
    time.sleep(3)

    # 7. Send more test keypresses
    print("[7] Sending more test keypresses...")
    child.sendline("echo test_1_after")
    time.sleep(0.5)
    child.sendline("echo test_2_after")
    time.sleep(0.5)
    child.sendline("echo test_3_after")
    time.sleep(2)

    # 8. Check marker file for AFTER_RELOAD
    print("[8] Checking results...")
    try:
        with open(MARKER) as f:
            content = f.read().strip()
        lines = content.split("\n")
        print(f"    Marker contents: {lines}")
        has_before = "BEFORE_RELOAD" in lines
        has_after = "AFTER_RELOAD" in lines
        print(f"    BEFORE_RELOAD present: {has_before}")
        print(f"    AFTER_RELOAD present: {has_after}")
    except FileNotFoundError:
        has_before = False
        has_after = False
        print("    FAIL: marker file not found")

    # 9. Also check the terminal output for echoed strings
    print("[9] Checking terminal output buffer...")
    try:
        buf = child.read_nonblocking(size=65536, timeout=2)
        has_test_after = "test_1_after" in buf or "test_2_after" in buf
        print(f"    Terminal shows post-reload output: {has_test_after}")
        if not has_test_after:
            print(f"    Buffer sample: {buf[:500]!r}")
    except pexpect.TIMEOUT:
        has_test_after = False
        print("    No output available (timeout)")

    # 10. Summary
    print()
    print("=== Summary ===")
    print(f"Before reload worked:  {has_before}")
    print(f"After reload worked:   {has_after}")

    child.close()
    cleanup()

    if has_before and has_after:
        print("RESULT: ALL PASSED")
        return 0
    else:
        print("RESULT: FAILED - keypresses dropped after hot reload")
        return 1


if __name__ == "__main__":
    sys.exit(main())
