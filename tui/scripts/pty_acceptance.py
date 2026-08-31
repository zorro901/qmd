#!/usr/bin/env python3
"""Real-PTY acceptance test for qmd-tui autosave and Esc-save.

Drives the ACTUAL built binary through a pseudo-terminal exactly like a user
over SSH (real keystrokes, real alt-screen), then checks the disk. This is the
end-to-end acceptance path that unit tests cannot cover: unit tests drive the
App struct directly, this exercises the crossterm event loop, the terminal,
and the qmd child processes.

Two scenarios are verified (the core save paths):

1. autosave  — press 'e', type a marker, wait past the 2s debounce WITHOUT
   pressing any save key, and confirm the marker is on disk BEFORE Esc.
2. esc-save  — open an EXISTING note with Enter, press 'e', type, press Esc,
   and confirm the marker lands on disk.

Edge cases (opt-in with --full): multi-line edit (Enter inside the editor
must NOT trigger open_selected), Ctrl-C in edit mode (saves & exits edit,
does NOT quit), and Ctrl-C outside edit mode (panic hatch, quits).

Requirements: python3, a qmd index with a test collection. Configure via env:

    QMD_TUI_BIN          path to the qmd-tui binary (default: ../bin/qmd-tui
                         relative to this script, i.e. the shipped binary)
    QMD_BIN              path to the qmd CLI (default: qmd on PATH)
    QMD_ACC_COLL_DIR     indexed collection dir to run in (created+indexed if
                        missing, reusing the TUI's own index env vars)

Usage (from tui/):
    QMD_BIN=/path/to/qmd python3 scripts/pty_acceptance.py [--full]

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
FULL = "--full" in sys.argv[1:]

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


def run_session(env, cwd, label, steps, kill=True):
    """Fork the TUI on a PTY, run steps(list of (delay, bytes_or_None)),
    return (accumulated_output, tui_pid, fd) with the process cleaned up
    (kill=True) or left running for the caller to inspect (kill=False)."""
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(env["QMD_TUI_BIN"], [env["QMD_TUI_BIN"]], env)
    # CRITICAL: give the PTY a real size or ratatui renders nothing.
    # Wide on purpose: the list pane is 40% of the width and the status text
    # (e.g. the delete confirmation prompt) lives at the END of the list
    # title, truncated away on narrow terminals. 240 cols keeps it visible.
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 240, 0, 0))

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
        if kill:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass
            os.close(fd)
    return out, pid, fd


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


def alive(pid):
    try:
        p, _ = os.waitpid(pid, os.WNOHANG)
        return p == 0
    except ChildProcessError:
        return False


def kill_session(pid, fd):
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    try:
        os.close(fd)
    except OSError:
        pass


def main():
    cwd = os.environ.get("QMD_ACC_COLL_DIR") or "/tmp/qmd-tui-acceptance"
    os.makedirs(cwd, exist_ok=True)
    env = build_env(cwd)
    if not os.path.exists(env["QMD_TUI_BIN"]):
        fail(f"TUI binary not found at {env['QMD_TUI_BIN']} (build it first)")
    ensure_collection(env, cwd)

    print(f"binary: {env['QMD_TUI_BIN']}")
    print(f"collection dir: {cwd}")
    results = {}

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
    results["autosave-before-esc"] = marker1 in open(note).read()
    print(f"[autosave] marker on disk BEFORE Esc: {results['autosave-before-esc']}")

    # --- Scenario 2: Esc saves on an existing note --------------------------
    marker2 = f"ESC-SAVE-OK-{os.getpid()}"
    with open(note, "w") as f:
        f.write("# acceptance baseline\n")

    run_session(env, cwd, "esc-save", [
        (3.0, None),                 # startup
        (0.5, b"\r"),                # Enter re-opens notes[0] explicitly
        (1.5, None),
        (0.5, b"e"),                 # edit
        (0.5, marker2.encode()),     # type marker (autosave may fire too)
        (0.6, None),
        (SAVE_POLL_SECS, b"\x1b"),   # Esc: save & exit; poll while draining
    ])
    results["esc-save-existing"] = marker2 in open(note).read()
    print(f"[esc-save] marker on disk after Esc: {results['esc-save-existing']}")

    # --- Edge cases (opt-in: --full) ----------------------------------------
    if FULL:
        # 3: Enter inside the editor inserts a newline; it must NOT open the
        # next list row. Two lines must be on disk afterwards.
        marker3 = f"MULTILINE-OK-{os.getpid()}"
        with open(note, "w") as f:
            f.write("# acceptance baseline\n")
        out3, pid, fd = run_session(env, cwd, "multiline", [
            (3.0, None),
            (1.0, b"e"),
            (0.5, marker3.encode()),
            (0.3, b"\r"),            # newline INSIDE the editor
            (0.5, b"line-two"),
            (SAVE_POLL_SECS, b"\x1b"),
        ], kill=False)
        kill_session(pid, fd)
        body = open(note).read()
        ok3 = marker3 in body and "line-two" in body and "\n" in body
        results["multiline-enter"] = ok3
        print(f"[multiline] Enter inserted a newline, both lines on disk: {ok3}")

        # 4: Ctrl-C in edit mode saves & exits edit mode; the app STAYS alive.
        marker4 = f"CTRLC-SAVE-OK-{os.getpid()}"
        with open(note, "w") as f:
            f.write("# acceptance baseline\n")
        out4, pid, fd = run_session(env, cwd, "ctrlc-edit", [
            (3.0, None),
            (1.0, b"e"),
            (0.5, marker4.encode()),
            (0.3, b"\x03"),          # Ctrl-C inside the editor
            (SAVE_POLL_SECS, None),
        ], kill=False)
        ok4 = marker4 in open(note).read() and alive(pid)
        print(f"[ctrl-c edit] saved and app still running: {ok4}")
        kill_session(pid, fd)
        results["ctrl-c-in-edit-saves"] = ok4

        # 5: Ctrl-C OUTSIDE edit mode quits immediately (panic hatch).
        out5, pid, fd = run_session(env, cwd, "ctrlc-quit", [
            (3.0, None),
            (1.0, b"\x03"),          # Ctrl-C on the list view
            (1.5, None),
        ], kill=False)
        quit_ok = not alive(pid)
        print(f"[ctrl-c list] app quit immediately: {quit_ok}")
        kill_session(pid, fd)
        results["ctrl-c-quit-hatch"] = quit_ok

        # 6: Mouse scenarios against the real binary via SGR sequences.
        # The list pane spans the full terminal height; its border is at
        # screen row 1, so list index i lives at row i+2. The dir holds two
        # notes (qmd-acc-note.md, qmd-acc-second.md), so valid rows are 2
        # (first note) and 3 (second note).
        marker6 = f"{os.getpid()}888"  # digits only: no key letters inside
        note2 = os.path.join(cwd, "qmd-acc-second.md")
        with open(note2, "w") as f:
            f.write(f"# second {marker6}\n")
        subprocess.run([env["QMD_BIN"], "update", "--path", note2],
                       env=env, check=False, cwd=cwd)

        def sgr_click(col, row, m=0):
            # SGR mouse encoding: ESC [ < b ; c ; r M (press) / m (release)
            return f"\x1b[<{m};{col};{row}M".encode() + \
                   f"\x1b[<{m};{col};{row}m".encode()

        out6, pid, fd = run_session(env, cwd, "mouse", [
            (3.0, None),             # startup: notes[0] previewed
            (1.0, None),
            # Click the SECOND note (row 3 => index 1) to select it.
            (2.5, sgr_click(10, 3)),
            # Double click it within the window -> inline editor opens.
            (0.8, sgr_click(10, 3)),
            # Type a digit-only marker; Esc saves.
            (1.0, marker6.encode()),
            (2.0, b"\x1b"),
        ], kill=False)
        edit_ok = False
        try:
            edit_ok = marker6 in open(note2).read() and alive(pid)
        except OSError:
            pass
        kill_session(pid, fd)
        print(f"[mouse] double click opened editor & saved marker: {edit_ok}")
        results["mouse-doubleclick-edit"] = edit_ok

        out7, pid, fd = run_session(env, cwd, "mouse-delete", [
            (3.0, None),
            (1.0, None),
            # Right click the SECOND note arms the delete confirmation. The
            # prompt lives in the list title (status bar), which differential
            # redraws render as fragments, so match a whitespace-free fragment.
            (2.0, sgr_click(10, 3, 2)),
        ], kill=False)
        flat = re.sub(r"\s+", "", vis(out7))
        # Differential redraws interleave status-bar cells, which escape
        # stripping scrambles ("delete this note?" may surface as
        # "delethisnote?" etc). Match fragments that survive mangling.
        prompt_visible = ("todelete" in flat and "anyother" in flat) or \
            "deletethisnote?" in flat or "delethisnote?" in flat
        print(f"[mouse] right click shows delete prompt: {prompt_visible}")
        kill_session(pid, fd)
        results["mouse-rightclick-delete-prompt"] = prompt_visible

    # --- Verdict -------------------------------------------------------------
    failed = [k for k, v in results.items() if not v]
    if not failed:
        print(f"PASS: all {len(results)} scenarios verified on the real binary")
        return 0
    fail(f"failed scenarios: {', '.join(failed)}")


if __name__ == "__main__":
    sys.exit(main())
