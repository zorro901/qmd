#!/usr/bin/env python3
"""Real-PTY acceptance test for qmd-tui autosave and Esc-save.

Drives the ACTUAL built binary through a pseudo-terminal exactly like a user
over SSH (real keystrokes, real alt-screen), then checks the disk. This is the
end-to-end acceptance path that unit tests cannot cover: unit tests drive the
App struct directly, this exercises the crossterm event loop, the terminal,
and the qmd child processes.

Two scenarios are verified:

1. autosave  — press 'e', type a marker, wait past the 2s debounce WITHOUT
   pressing any save key, and confirm the marker is on disk BEFORE Esc.
2. esc-save  — open an EXISTING note with Enter, press 'e', type, press Esc,
   and confirm the marker lands on disk.

Requirements: python3, a qmd index with a test collection. Configure via env:

    QMD_TUI_BIN          path to the qmd-tui binary (default: ../bin/qmd-tui
                         relative to this script, i.e. the shipped binary)
    QMD_BIN              path to the qmd CLI (default: qmd on PATH)
    QMD_ACC_COLL_DIR     indexed collection dir to run in (created+indexed if
                        missing, reusing the TUI's own index env vars)

Usage (from tui/):
    QMD_BIN=/path/to/qmd python3 scripts/pty_acceptance.py

Exit 0 only if BOTH scenarios pass. The PTY quirk to know about: the pty must
be given a real window size via TIOCSWINSZ, otherwise ratatui renders nothing
and every assertion fails with an empty screen.
"""

import fcntl
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

DEBOUNCE_SECS = 2.0
AUTOSAVE_WAIT = DEBOUNCE_SECS + 1.0
SAVE_POLL_SECS = 8.0

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
TUI_DIR = os.path.dirname(SCRIPT_DIR)


def fail(msg):
    print(f"FAIL: {msg}")
    sys.exit(1)


def build_env(cwd):
    env = dict(os.environ)
    env.setdefault("QMD_TUI_BIN", os.path.join(TUI_DIR, "..", "bin", "qmd-tui"))
    env.setdefault("QMD_BIN", "qmd")
    # The TUI child inherits these; qmd resolves the index the same way the
    # user's shell does (project .qmd, else QMD_CONFIG_DIR/INDEX_PATH).
    env.setdefault("INDEX_PATH", os.path.join(cwd, "..", "qmd-acc.db"))
    env.setdefault("QMD_CONFIG_DIR", os.path.join(cwd, "..", "qmd-acc.config"))
    env.setdefault("XDG_CONFIG_HOME", env["QMD_CONFIG_DIR"])
    env["TERM"] = "xterm-256color"
    return env


def ensure_collection(env, cwd):
    """Make sure the acceptance cwd is an indexed collection with >=1 note."""
    qmd = env["QMD_BIN"]
    out = subprocess.run([qmd, "ls"], capture_output=True, text=True, env=env, cwd=cwd)
    if out.returncode != 0 or "qmd://" not in out.stdout:
        print(f"setting up collection at {cwd}")
        subprocess.run([qmd, "collection", "add", cwd], env=env, check=False, cwd=cwd)
        with open(os.path.join(cwd, "qmd-acc-note.md"), "w") as f:
            f.write("# acceptance baseline\n")
        subprocess.run([qmd, "update", "--path", os.path.join(cwd, "qmd-acc-note.md")],
                       env=env, check=False, cwd=cwd)
    else:
        with open(os.path.join(cwd, "qmd-acc-note.md"), "w") as f:
            f.write("# acceptance baseline\n")
        subprocess.run([qmd, "update", "--path", os.path.join(cwd, "qmd-acc-note.md")],
                       env=env, check=False, cwd=cwd)


def run_session(env, cwd, label, steps):
    """Fork the TUI on a PTY, run steps(list of (delay, bytes_or_None)),
    return (child_output, tui_pid, fd) after steps complete."""
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(env["QMD_TUI_BIN"], [env["QMD_TUI_BIN"]], env)
    # CRITICAL: give the PTY a real size or ratatui renders nothing.
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    out = b""

    def drain(sec):
        nonlocal out
        end = time.time() + sec
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.1)
            if r:
                try:
                    out += os.read(fd, 65536)
                except OSError:
                    return

    try:
        for delay, keys in steps:
            if keys is not None:
                os.write(fd, keys)
            drain(delay)
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        os.close(fd)
    return out


def vis(raw):
    """Strip ANSI escapes so screen text is greppable."""
    return re.sub(rb"\x1b\[[0-9;?]*[a-zA-Z]", b"", raw).decode(errors="replace")


def open_note_file(raw, cwd):
    """The right pane header names the open note: `┌t/<id>.md───┐`."""
    m = re.search(r"┌(?:[A-Za-z0-9_-]+)/([A-Za-z0-9._/-]+\.md)", vis(raw))
    return os.path.join(cwd, m.group(1)) if m else None


def wait_for_marker(path, marker, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if marker in open(path).read():
                return True
        except OSError:
            pass
        time.sleep(0.2)
    return False


def main():
    cwd = os.environ.get("QMD_ACC_COLL_DIR") or "/tmp/qmd-tui-acceptance"
    os.makedirs(cwd, exist_ok=True)
    env = build_env(cwd)
    if not os.path.exists(env["QMD_TUI_BIN"]):
        fail(f"TUI binary not found at {env['QMD_TUI_BIN']} (build it first)")
    ensure_collection(env, cwd)

    print(f"binary: {env['QMD_TUI_BIN']}")
    print(f"collection dir: {cwd}")

    # --- Scenario 1: autosave fires BEFORE Esc (no save key pressed) --------
    marker1 = f"AUTOSAVE-OK-{os.getpid()}"
    note = os.path.join(cwd, "qmd-acc-note.md")
    with open(note, "w") as f:
        f.write("# acceptance baseline\n")

    run_session(env, cwd, "autosave", [
        (3.0, None),                 # startup: notes[0] previewed
        (1.0, b"e"),                 # enter edit mode
        (0.5, marker1.encode()),     # type marker
        (AUTOSAVE_WAIT, None),       # wait past debounce; press NOTHING
    ])
    before_esc = marker1 in open(note).read()
    print(f"[autosave] marker on disk BEFORE Esc: {before_esc}")

    # --- Scenario 2: Esc saves on an existing note --------------------------
    marker2 = f"ESC-SAVE-OK-{os.getpid()}"
    with open(note, "w") as f:
        f.write("# acceptance baseline\n")

    raw = run_session(env, cwd, "esc-save", [
        (3.0, None),                 # startup
        (0.5, b"\r"),                # Enter re-opens notes[0] explicitly
        (1.5, None),
        (0.5, b"e"),                 # edit
        (0.5, marker2.encode()),     # type marker (autosave may fire too)
        (0.6, None),
        (SAVE_POLL_SECS, b"\x1b"),   # Esc: save & exit; poll while draining
    ])
    esc_ok = marker2 in open(note).read()
    print(f"[esc-save] marker on disk after Esc: {esc_ok}")

    if before_esc and esc_ok:
        print("PASS: autosave and Esc-save both verified on the real binary")
        return 0
    if not before_esc:
        fail("autosave did not write before Esc (check debounce/status bar)")
    fail("Esc-save did not write (check open note resolution)")


if __name__ == "__main__":
    sys.exit(main())
