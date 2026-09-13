"""Exercise Wakterm-generated restore argv against a real native Wsh PTY."""
import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import sys
import tempfile
import time


def application():
    mode = sys.argv[2]
    assert os.tcgetpgrp(0) == os.getpgrp(), "application is not foreground"
    print("APP_ARGS:" + json.dumps([os.fsencode(arg).hex() for arg in sys.argv[3:]]), flush=True)
    if mode == "exit":
        sys.exit(37)
    signal.signal(signal.SIGCONT, lambda *_: print("APP_RESUMED", flush=True))
    signal.signal(signal.SIGINT, lambda *_: sys.exit(130))
    print("APP_READY", flush=True)
    while True:
        signal.pause()


def verify():
    executable, *arguments = sys.argv[1:]
    expected = [b"", b"argument with spaces", b"$(false); quote'\"",
                b"line\nbreak", b"--literal-option", b"\xffx"]
    mode = arguments[arguments.index("--application") + 1]
    with tempfile.TemporaryDirectory(prefix="wakterm-wsh-restore-") as scratch:
        Path(scratch, ".zshrc").write_text("WSH_THEME=''\nPROMPT='WAKTERM_RESTORE> '\nRPROMPT=''\n")
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("WSH_", "ZSH_", "WAKTERM_"))
            and key not in ("ENV", "BASH_ENV", "ZDOTDIR")
        }
        environment.update(HOME=scratch, ZDOTDIR=scratch, WSH_THEME="", TERM="xterm-256color")
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(scratch)
            os.execve(executable, [executable, *arguments], environment)
        output = bytearray()

        def wait(marker, offset=0):
            deadline = time.monotonic() + 10
            while marker not in output[offset:]:
                assert time.monotonic() < deadline, ("PTY timeout", marker, bytes(output))
                if select.select([fd], [], [], 0.1)[0]:
                    try:
                        chunk = os.read(fd, 65536)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        chunk = b""
                    assert chunk, ("shell exited before expected output", marker, bytes(output))
                    output.extend(chunk)

        try:
            wait(b"APP_ARGS:" + json.dumps([arg.hex() for arg in expected]).encode())
            if mode == "interrupt":
                wait(b"APP_READY")
                job_group = os.tcgetpgrp(fd)
                assert job_group != pid, "application shares the shell process group"
                offset = len(output)
                os.write(fd, b"\x1a")
                wait(b"WAKTERM_RESTORE> ", offset)
                assert os.tcgetpgrp(fd) == pid, "shell did not regain the terminal"
                offset = len(output)
                os.write(fd, b"fg\r")
                wait(b"APP_RESUMED", offset)
                assert os.tcgetpgrp(fd) == job_group, "fg did not resume the same job"
                offset = len(output)
                os.write(fd, b"\x03")
                wait(b"WAKTERM_RESTORE> ", offset)
            else:
                wait(b"WAKTERM_RESTORE> ")
            assert os.tcgetpgrp(fd) == pid, "prompt is not in foreground"
            offset = len(output)
            os.write(fd, b"printf 'RESTORE_%s:%s\\n' STATUS $?; exit 0\r")
            wait(b"RESTORE_STATUS:" + (b"130" if mode == "interrupt" else b"37"), offset)
            deadline = time.monotonic() + 5
            while True:
                waited, status = os.waitpid(pid, os.WNOHANG)
                if waited:
                    pid = None
                    assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, status
                    break
                assert time.monotonic() < deadline, "shell did not exit"
                time.sleep(0.01)
            print(f"PASS: {mode}: exact argv bytes, foreground ownership, prompt return and exit status")
        finally:
            if pid is not None:
                foreground = os.tcgetpgrp(fd)
                if foreground > 0 and foreground != os.getpgrp():
                    try:
                        os.killpg(foreground, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.waitpid(pid, 0)
            os.close(fd)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--application"]:
        application()
    else:
        verify()
