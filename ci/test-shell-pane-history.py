#!/usr/bin/env python3
"""Exercise wakterm.sh pane-local history in real interactive bash and zsh.

Usage: ci/test-shell-pane-history.py assets/shell-integration/wakterm.sh [zsh] [bash]
"""

import os
import pty
import re
import select
import signal
import sys
import tempfile
import time
import uuid
from pathlib import Path

SCRIPT = Path(sys.argv[1]).resolve()
SHELLS = sys.argv[2:] or ["zsh", "bash"]


def drain(fd, seconds):
    end = time.time() + seconds
    out = b""
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], 0.05)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
    return out


def run(shell, home, token, commands, kill=False):
    env = {
        "HOME": str(home),
        "TERM": "xterm-256color",
        "PATH": os.environ["PATH"],
        "XDG_STATE_HOME": str(home / "state"),
        "WAKTERM_PANE_TOKEN": token,
    }
    if shell == "zsh":
        env["ZDOTDIR"] = str(home)
        argv = ["zsh", "-i"]
    else:
        argv = ["bash", "--rcfile", str(home / ".bashrc"), "-i"]
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(argv[0], argv, env)
    drain(fd, 1.5)
    for command in commands:
        os.write(fd, command.encode() + b"\n")
        drain(fd, 0.6)
    if kill:
        os.kill(pid, signal.SIGKILL)
    else:
        os.write(fd, b"exit\n")
    drain(fd, 1.0)
    os.waitpid(pid, 0)
    os.close(fd)


def entries(path):
    if not path.exists():
        return []
    lines = []
    for line in path.read_text().splitlines():
        line = re.sub(r"^: \d+:\d+;", "", line)
        # Each run ends with an `exit` command, which shells record too.
        if line and not line.startswith("#") and line != "exit":
            lines.append(line)
    return lines


def check(name, actual, expected):
    status = "ok" if actual == expected else "FAIL"
    print(f"  {status}: {name}: {actual!r}" + ("" if actual == expected else f" != {expected!r}"))
    return actual == expected


def scenario(shell):
    print(f"{shell}:")
    ok = True
    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp)
        shared = home / (".zsh_history" if shell == "zsh" else ".bash_history")
        shared.write_text("echo global-old\n")
        if shell == "zsh":
            (home / ".zshrc").write_text(
                f"HISTFILE=$HOME/.zsh_history\nHISTSIZE=1000\nSAVEHIST=1000\n"
                f"setopt share_history extended_history\nsource {SCRIPT}\n"
            )
        else:
            (home / ".bashrc").write_text(
                f"HISTFILE=$HOME/.bash_history\nHISTSIZE=1000\nHISTFILESIZE=1000\nsource {SCRIPT}\n"
            )
        t1, t2 = str(uuid.uuid4()), str(uuid.uuid4())
        pane = lambda token: home / "state/wakterm/pane-history" / f"{token}.{shell}"
        dump = "fc -ln 1 >| $HOME/list" if shell == "zsh" else "history >| $HOME/list"

        run(shell, home, t1, ["echo a1"])
        run(shell, home, t2, ["echo b1"])
        ok &= check("pane 1 file", entries(pane(t1)), ["echo a1"])
        ok &= check("pane 2 file", entries(pane(t2)), ["echo b1"])
        ok &= check("shared file", entries(shared), ["echo global-old", "echo a1", "echo b1"])

        # A later shell in pane 1 recalls pane 1 first, then shared history.
        run(shell, home, t1, [dump])
        listed = [re.sub(r"^\s*\d+\s+", "", l).strip() for l in (home / "list").read_text().splitlines()]
        listed = [l for l in listed if l and l not in (dump, "exit")]
        ok &= check("recall order in pane 1", listed[-2:], ["echo b1", "echo a1"])

        # A killed shell leaves its commands for the next shell in the pane.
        run(shell, home, t1, ["echo a3"], kill=True)
        ok &= check("leftover after kill", entries(Path(f"{pane(t1)}.new")), ["echo a3"])
        run(shell, home, t1, [])
        ok &= check("leftover merged into pane", entries(pane(t1))[-1:], ["echo a3"])
        ok &= check("leftover merged into shared", entries(shared)[-1:], ["echo a3"])
        ok &= check("leftover removed", Path(f"{pane(t1)}.new").exists(), False)

        # A nested interactive shell uses shared history, not the pane's files.
        nested = "zsh -i" if shell == "zsh" else f"bash --rcfile $HOME/.bashrc -i"
        run(shell, home, t1, [nested, "echo nested1", "exit", "echo a5"])
        ok &= check("nested command kept out of pane", "echo nested1" in entries(pane(t1)), False)
        ok &= check("outer command in pane", entries(pane(t1))[-1:], ["echo a5"])
        ok &= check("nested command in shared", "echo nested1" in entries(shared), True)
    return ok


results = [scenario(shell) for shell in SHELLS]
sys.exit(0 if all(results) else 1)
