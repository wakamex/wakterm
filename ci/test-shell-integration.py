#!/usr/bin/env python3
"""Exercise Wakterm's automatic shell integration in real bash, zsh and fish.

Each shell is started the way Wakterm's LocalDomain starts it with
`shell_integration` enabled (see mux/src/shell_integration.rs), inside a PTY
with an isolated HOME whose startup files record that they ran.

Usage: ci/test-shell-integration.py [--fish PATH] [--wsh PATH] [zsh] [bash] [fish] [wsh]
"""

import argparse
import os
import pty
import re
import select
import shutil
import signal
import sys
import tempfile
import time
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
FILES = {
    "wakterm.sh": REPO / "assets/shell-integration/wakterm.sh",
    "zsh/.zshenv": REPO / "assets/shell-integration-inject/zsh/.zshenv",
    "bash/inject.bash": REPO / "assets/shell-integration-inject/bash/inject.bash",
    "fish/fish/vendor_conf.d/wakterm.fish": REPO
    / "assets/shell-integration-inject/fish/fish/vendor_conf.d/wakterm.fish",
}
REEXEC = 'eval "${__wakterm_reexec:-exec \\"\\$0\\" -l}"'


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
            # Answer a primary device attributes query as a terminal does;
            # fish waits for the reply before it reads input.
            if b"\x1b[0c" in chunk or b"\x1b[c" in chunk:
                os.write(fd, b"\x1b[?62;22c")
            out += chunk
    return out


class Env:
    def __init__(self, root, shell, shell_path):
        self.shell = shell
        self.shell_path = shell_path
        self.home = root / "home"
        self.home.mkdir()
        self.dir = root / "integration"
        for name, source in FILES.items():
            target = self.dir / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(source, target)
        self.markers = self.home / "markers"
        self.state = self.home / "state"

    def base_env(self, token):
        return {
            "HOME": str(self.home),
            "TERM": "xterm-256color",
            "PATH": os.environ["PATH"],
            "SHELL": self.shell_path,
            "XDG_STATE_HOME": str(self.state),
            "XDG_DATA_HOME": str(self.home / "data"),
            "WAKTERM_PANE_TOKEN": token,
        }

    def command(self, token, wrapper=False, extra_env=None):
        """Argv, argv[0] and environment as Wakterm would spawn them."""
        env = self.base_env(token)
        env.update(extra_env or {})
        env["WAKTERM_SHELL_INTEGRATION_DIR"] = str(self.dir)
        path = self.shell_path
        name = os.path.basename(path)
        if self.shell in ("zsh", "wsh"):
            if "ZDOTDIR" in env:
                env["WAKTERM_ORIG_ZDOTDIR"] = env["ZDOTDIR"]
            env["ZDOTDIR"] = str(self.dir / "zsh")
            if wrapper:
                return [path, "-l", "-i", "-c", f"true; {REEXEC}", path], path, env
            return [path], f"-{name}", env
        if self.shell == "bash":
            env["ENV"] = str(self.dir / "bash/inject.bash")
            env["WAKTERM_BASH_LOGIN"] = "1"
            env["HISTFILE"] = str(self.home / ".bash_history")
            env["WAKTERM_BASH_UNEXPORT_HISTFILE"] = "1"
            if wrapper:
                return [path, "--posix", "-i", "-c", f"true; {REEXEC}", path], path, env
            return [path, "--posix"], path, env
        env["XDG_DATA_DIRS"] = f"{self.dir / 'fish'}:/usr/local/share:/usr/share"
        return [path], f"-{name}", env

    def run(self, token, commands, kill=False, wrapper=False, extra_env=None):
        argv, arg0, env = self.command(token, wrapper, extra_env)
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(argv[0], [arg0] + argv[1:], env)
        out = drain(fd, 2.0)
        for command in commands:
            os.write(fd, command.encode() + b"\n")
            out += drain(fd, 0.7)
        if kill:
            os.kill(pid, signal.SIGKILL)
        else:
            os.write(fd, b"exit\n")
        out += drain(fd, 1.0)
        os.waitpid(pid, 0)
        os.close(fd)
        return out.decode(errors="replace")

    def read_markers(self):
        if not self.markers.exists():
            return []
        lines = self.markers.read_text().splitlines()
        self.markers.unlink()
        return lines


def history_entries(path, shell):
    if not path.exists():
        return []
    text = path.read_text()
    if shell == "fish":
        lines = re.findall(r"^- cmd: (.*)$", text, re.M)
    else:
        lines = [re.sub(r"^: \d+:\d+;", "", l) for l in text.splitlines()]
    return [l for l in lines if l and not l.startswith("#") and l != "exit"]


class Checks:
    def __init__(self, shell):
        self.ok = True
        print(f"{shell}:")

    def __call__(self, name, actual, expected):
        good = actual == expected
        self.ok &= good
        suffix = "" if good else f" != {expected!r}"
        print(f"  {'ok' if good else 'FAIL'}: {name}: {actual!r}{suffix}")


def write_startup_files(env):
    m = env.markers
    if env.shell in ("zsh", "wsh"):
        for name in [".zshenv", ".zprofile", ".zlogin"]:
            (env.home / name).write_text(f"echo {name} >> {m}\n")
        (env.home / ".zshrc").write_text(
            f"echo .zshrc >> {m}\nHISTFILE=$HOME/.zsh_history\nHISTSIZE=1000\nSAVEHIST=1000\n"
            f"setopt share_history extended_history\n"
            # A user who also sources the script must not get duplicate hooks.
            f"source {REPO / 'assets/shell-integration/wakterm.sh'}\n"
        )
        custom = env.home / "zdot"
        custom.mkdir()
        (custom / ".zshenv").write_text(f"echo custom-zshenv >> {m}\n")
        (custom / ".zshrc").write_text(f"echo custom-zshrc >> {m}\n")
    elif env.shell == "bash":
        (env.home / ".bash_profile").write_text(
            f"echo .bash_profile >> {m}\nsource $HOME/.bashrc\n"
        )
        (env.home / ".profile").write_text(f"echo .profile >> {m}\n")
        (env.home / ".bashrc").write_text(f"echo .bashrc >> {m}\nHISTSIZE=1000\n")
    else:
        config = env.home / ".config/fish"
        config.mkdir(parents=True)
        (config / "config.fish").write_text(f"echo config.fish >> {m}\n")


def probe(env, check, token, wrapper=False):
    """Check startup files, the restored environment and integration."""
    if env.shell in ("zsh", "wsh"):
        commands = [
            'echo "ZDOTDIR=${ZDOTDIR-unset}" >> $HOME/markers',
            'echo "hooks=${(M)#precmd_functions:#__wakterm_osc7}" >> $HOME/markers',
            'echo "user_vars=${(M)#precmd_functions:#__wakterm_user_vars_precmd}" >> $HOME/markers',
        ]
    elif env.shell == "bash":
        commands = [
            'echo "ENV=${ENV-unset} posix=$(shopt -po posix)" >> $HOME/markers',
            # Pane history captured the shell's HISTFILE as the shared file.
            'echo "HISTFILE=$__wakterm_shared_histfile exported=$([[ ${HISTFILE@a} == *x* ]] && echo yes || echo no)" >> $HOME/markers',
            'n=0; for f in "${precmd_functions[@]}"; do [[ $f == __wakterm_osc7 ]] && ((n++)); done; echo "hooks=$n" >> $HOME/markers',
        ]
    else:
        commands = [
            'echo "injected="(string match -q "*integration*" -- "$XDG_DATA_DIRS"; and echo yes; or echo no) >> $HOME/markers',
            'echo "history="$fish_history >> $HOME/markers',
        ]
    env.run(token, commands, wrapper=wrapper)
    return env.read_markers()


def scenario(shell, shell_path):
    check = Checks(shell)
    with tempfile.TemporaryDirectory() as tmp:
        env = Env(Path(tmp), shell, shell_path)
        write_startup_files(env)
        token = str(uuid.uuid4())
        pane_session = "wakterm_" + token.replace("-", "_")

        markers = probe(env, check, token)
        if shell == "wsh":
            check("login startup files", markers[:4], [".zshenv", ".zprofile", ".zshrc", ".zlogin"])
            check("ZDOTDIR restored", markers[4], "ZDOTDIR=unset")
            check("native OSC 7 and 133 kept", markers[5], "hooks=0")
            check("Wakterm user vars loaded once", markers[6], "user_vars=1")
            env.run(token, ["echo w1"])
            check("Wsh owns pane history", (env.state / "wakterm/pane-history").exists(), False)
            return check.ok
        if shell == "zsh":
            check("login startup files", markers[:4], [".zshenv", ".zprofile", ".zshrc", ".zlogin"])
            check("ZDOTDIR restored", markers[4], "ZDOTDIR=unset")
            check("hooks installed once", markers[5], "hooks=1")
            check("user vars installed once", markers[6], "user_vars=1")
            env.run(token, [], extra_env={"ZDOTDIR": str(env.home / "zdot")})
            custom = env.read_markers()
            check("custom ZDOTDIR startup files", custom[:2], ["custom-zshenv", "custom-zshrc"])
        elif shell == "bash":
            check("login startup files", markers[:2], [".bash_profile", ".bashrc"])
            check("POSIX mode off and ENV restored", markers[2], "ENV=unset posix=set +o posix")
            check("HISTFILE default kept private", markers[3], f"HISTFILE={env.home}/.bash_history exported=no")
            check("hooks installed once", markers[4], "hooks=1")
        else:
            check("config.fish ran", markers[:1], ["config.fish"])
            check("injection removed from XDG_DATA_DIRS", markers[1], "injected=no")
            check("pane history session", markers[2], f"history={pane_session}")

        wrapped = probe(env, check, token, wrapper=True) if shell != "fish" else None
        if shell == "zsh":
            # The -c shell and the login shell it execs each read the files.
            check("restore wrapper ends in an integrated login shell", wrapped[-3:], ["ZDOTDIR=unset", "hooks=1", "user_vars=1"])
            check("login shell after the agent read .zlogin", wrapped.count(".zlogin"), 2)
        elif shell == "bash":
            check("restore wrapper ends in an integrated login shell", wrapped[-3:], [
                "ENV=unset posix=set +o posix",
                f"HISTFILE={env.home}/.bash_history exported=no",
                "hooks=1",
            ])

        # Pane history through injection alone.
        state = env.state / "wakterm/pane-history"
        data = env.home / "data/fish"
        if shell == "fish":
            shared = data / "fish_history"
            shared.parent.mkdir(parents=True, exist_ok=True)
            shared.write_text("- cmd: echo global-old\n  when: 1\n")
            pane = lambda t: data / f"wakterm_{t.replace('-', '_')}_history"
        else:
            shared = env.home / (".zsh_history" if shell == "zsh" else ".bash_history")
            shared.write_text("echo global-old\n")
            pane = lambda t: state / f"{t}.{shell}"
        for leftover in state.glob("*") if state.exists() else []:
            leftover.unlink()
        for leftover in data.glob("wakterm_*") if data.exists() else []:
            leftover.unlink()
        t1, t2 = str(uuid.uuid4()), str(uuid.uuid4())
        env.run(t1, ["echo a1"])
        env.run(t2, ["echo b1"])
        check("pane 1 history", history_entries(pane(t1), shell)[-1:], ["echo a1"])
        check("pane 2 history", history_entries(pane(t2), shell)[-1:], ["echo b1"])
        check("shared history", history_entries(shared, shell), ["echo global-old", "echo a1", "echo b1"])
        env.run(t1, ["echo a3"], kill=True)
        env.run(t1, [])
        check("killed shell merged by the next one", history_entries(shared, shell)[-1:], ["echo a3"])
        check("killed shell's command kept in its pane", history_entries(pane(t1), shell)[-1:], ["echo a3"])
        check("other pane unaffected", history_entries(pane(t2), shell)[-1:], ["echo b1"])
    return check.ok


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fish", default=shutil.which("fish"))
    parser.add_argument("--wsh", default=shutil.which("wsh"))
    parser.add_argument("shells", nargs="*", default=["zsh", "bash", "fish", "wsh"])
    args = parser.parse_args()
    ok = True
    for shell in args.shells:
        path = {"fish": args.fish, "wsh": args.wsh}.get(shell) or shutil.which(shell)
        if not path:
            print(f"{shell}: skipped, not installed")
            continue
        ok &= scenario(shell, str(Path(path).resolve()))
    sys.exit(0 if ok else 1)


main()
