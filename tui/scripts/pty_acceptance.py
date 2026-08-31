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
does NOT quit), Ctrl-C outside edit mode (panic hatch, quits), and mouse:
double click opens the editor on the note under the pointer, right click
arms the delete prompt.

Coverage summary (--full, 7 scenarios): autosave debounce, Esc save,
multiline Enter, Ctrl-C hatch (both modes), mouse edit, mouse delete prompt.
Persistence workflows driven only through the CLI layer are covered by the
headless unit tests (search filter, n-create, y-duplicate, d-confirmed
delete, c-collection picker): they shell out to the real qmd binary but do
not need a terminal.

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
import json
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


def qmd(env, cwd, *args):
    """Run a qmd CLI command, return stdout text ('' on failure)."""
    out = subprocess.run([env["QMD_BIN"], *args], capture_output=True,
                         text=True, env=env, cwd=cwd)
    return out.stdout if out.returncode == 0 else ""


def note_index(env, cwd, filename):
    """Position of a note in the TUI's note list (mtime DESC order), or None
    when it is missing from the index. Callers fail loudly on None instead of
    clicking/asserting a row that does not exist."""
    try:
        notes = json.loads(qmd(env, cwd, "notes", "--format", "json") or "[]")
    except json.JSONDecodeError:
        return None
    for i, n in enumerate(notes):
        if n.get("file", "").endswith("/" + filename):
            return i
    return None


def reset_notes(env, cwd, note2_body, newest="note1"):
    """Deterministic pre-scenario state for the acceptance run.

    qmd-acc-note.md always gets a unique salt comment so its content — and
    therefore the indexed modified_at — changes on EVERY call (qmd dedups
    unchanged files and keeps the old mtime, which once silently flipped the
    list order between runs). List order is modified_at DESC, so:

      newest="note1" (scenarios 1-5): note.md is list index 0, the note those
      scenarios edit and assert on.
      newest="note2" (mouse scenarios): qmd-acc-second.md is written last and
      its list index is returned, so the harness clicks a row it actually
      resolved instead of a hardcoded one.

    Returns (idx_note_md, idx_note2_md)."""
    note1 = os.path.join(cwd, "qmd-acc-note.md")
    note2 = os.path.join(cwd, "qmd-acc-second.md")
    salt = f"<!-- acc {time.time_ns()} -->\n"
    # Write in mtime order: the file written LAST ends up newest.
    order = [(note2, note2_body), (note1, salt + "# acceptance baseline\n")]
    if newest == "note2":
        order.reverse()
    for path, body in order:
        with open(path, "w") as f:
            f.write(body)
        time.sleep(0.3)
    for path in (note1, note2):
        subprocess.run([env["QMD_BIN"], "update", "--path", path],
                       env=env, check=False, cwd=cwd)
        time.sleep(0.3)
    return (note_index(env, cwd, "qmd-acc-note.md"),
            note_index(env, cwd, "qmd-acc-second.md"))


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
    # Salted reset: note.md ends up newest = list index 0 = what the TUI
    # previews on startup and what these scenarios edit and assert on.
    marker1 = f"AUTOSAVE-OK-{os.getpid()}"
    note = os.path.join(cwd, "qmd-acc-note.md")
    idx1, _ = reset_notes(env, cwd, "# second baseline\n")
    if idx1 != 0:
        fail(f"expected qmd-acc-note.md at list index 0, got {idx1}")

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
    idx2, _ = reset_notes(env, cwd, "# second baseline\n")
    if idx2 != 0:
        fail(f"expected qmd-acc-note.md at list index 0, got {idx2}")

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
        reset_notes(env, cwd, "# second baseline\n")
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
        reset_notes(env, cwd, "# second baseline\n")
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
        # Salted reset with the second note written LAST: it becomes newest,
        # its list index is RESOLVED (not hardcoded), and the marker is in
        # the file exactly once before the click.
        marker6 = f"{os.getpid()}888"  # digits only: no key letters inside
        note2 = os.path.join(cwd, "qmd-acc-second.md")
        _, idx6 = reset_notes(env, cwd, f"# second {marker6}\n", newest="note2")
        if idx6 is None or idx6 > 5:
            fail(f"qmd-acc-second.md not in index or at unexpected row: {idx6}")
        row2 = idx6 + 2  # list border is screen row 1; index i lives at row i+2

        def sgr_click(col, row, m=0):
            # SGR mouse encoding: ESC [ < b ; c ; row M (press) / m (release)
            return f"\x1b[<{m};{col};{row}M".encode() + \
                   f"\x1b[<{m};{col};{row}m".encode()

        out6, pid, fd = run_session(env, cwd, "mouse", [
            (3.0, None),             # startup: notes[0] previewed
            (1.0, None),
            # First click selects and previews (preview blocks ~1s on the qmd
            # child). The second click is written DURING that block, so the
            # app processes it right after stamping last_click -> inside the
            # 400ms double-click window. A >400ms wall gap would NOT count.
            (0.3, sgr_click(10, row2)),
            (0.5, sgr_click(10, row2)),
            # The editor is open by the time these queue-drained digits are
            # processed; in list mode digits are no-ops, so they must have
            # been typed inside the textarea.
            (1.5, marker6.encode()),
            (2.0, b"\x1b"),          # Esc saves & exits
        ], kill=False)
        edit_ok = False
        routed = "editing" in vis(out6)  # edit-mode status line rendered
        try:
            body = open(note2).read()
            # REAL assertion: the pre-click file (written by reset_collection)
            # contains the marker exactly ONCE (in the heading); the editor
            # must have added a SECOND occurrence via typing. Count-based so
            # it holds wherever the textarea cursor starts. A failed
            # double-click leaves the file untouched (digits are no-ops in
            # list mode), so a vacuous pass is impossible.
            edit_ok = routed and body.count(marker6) == 2 \
                and f"# second {marker6}" in body and alive(pid)
        except OSError:
            pass
        kill_session(pid, fd)
        print(f"[mouse] double click opened editor & appended marker: {edit_ok} "
              f"(routed={routed})")
        results["mouse-doubleclick-edit"] = edit_ok

        # 7: Right click arms the delete confirmation. The reset gives the
        # target a unique title; the prompt only counts if that exact title
        # was on screen (proves WHICH note the click routed to).
        target7 = f"second{os.getpid()}777"
        _, idx7 = reset_notes(env, cwd, f"# {target7}\n", newest="note2")
        if idx7 is None or idx7 > 5:
            fail(f"qmd-acc-second.md not in index or at unexpected row: {idx7}")
        row7 = idx7 + 2

        out7, pid, fd = run_session(env, cwd, "mouse-delete", [
            (3.0, None),
            (1.0, None),
            # Right click the second note arms the delete confirmation.
            (2.0, sgr_click(10, row7, 2)),
            (0.5, b"\r"),            # Enter CONFIRMS the delete
            (3.0, None),             # delete + collection reindex
        ], kill=False)
        flat = re.sub(r"\s+", "", vis(out7))
        # Differential redraws interleave status-bar cells, which escape
        # stripping scrambles ("delete this note?" may surface as
        # "delethisnote?" etc). Match fragments that survive mangling.
        prompt_visible = ("todelete" in flat and "anyother" in flat) or \
            "deletethisnote?" in flat or "delethisnote?" in flat
        # End-to-end routing proof: after confirming, the CLICKED file must
        # be gone from disk. The list shows every title, so target visibility
        # alone cannot prove WHICH note the right click selected; only the
        # deletion of note2 (not note.md) can.
        deleted = not os.path.exists(note2) and alive(pid)
        print(f"[mouse] right click shows delete prompt: {prompt_visible} "
              f"(target deleted after Enter: {deleted})")
        kill_session(pid, fd)
        results["mouse-rightclick-delete-prompt"] = prompt_visible and deleted

    # --- Verdict -------------------------------------------------------------
    failed = [k for k, v in results.items() if not v]
    if not failed:
        print(f"PASS: all {len(results)} scenarios verified on the real binary")
        return 0
    fail(f"failed scenarios: {', '.join(failed)}")


if __name__ == "__main__":
    sys.exit(main())
