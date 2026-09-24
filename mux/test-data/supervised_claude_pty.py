"""Start an offline native Claude TUI for Wakterm's Linux observer regression."""

import datetime
import json
import os
from pathlib import Path
import pty
import select
import signal
import sys
import time
import uuid

root = Path(sys.argv[1])
binary = Path(sys.argv[2]).resolve()
work = root / "work"
config = root / "claude"
work.mkdir()
config.mkdir()
session_id = str(uuid.uuid4())
turn_id = str(uuid.uuid4())
project = config / "projects" / str(work).replace("/", "-")
project.mkdir(parents=True)
session = project / f"{session_id}.jsonl"
now = datetime.datetime.now(datetime.timezone.utc).isoformat()
records = [
    {"type": "user", "uuid": turn_id, "sessionId": session_id, "cwd": str(work), "timestamp": now,
     "message": {"role": "user", "content": "Reply ready."}},
    {"type": "assistant", "uuid": str(uuid.uuid4()), "parentUuid": turn_id, "sessionId": session_id,
     "cwd": str(work), "timestamp": now, "message": {"id": "msg_fixture", "role": "assistant",
     "model": "claude-sonnet-4-6", "stop_reason": "end_turn", "content": [{"type": "text", "text": "ready"}]}},
]
session.write_text("".join(json.dumps(record) + "\n" for record in records))
(config / ".claude.json").write_text(json.dumps({
    "hasCompletedOnboarding": True, "theme": "dark",
    "projects": {str(work): {"hasTrustDialogAccepted": True}},
}))
args = [
    "bwrap", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--unshare-net",
    "--die-with-parent", "--clearenv", "--ro-bind", "/usr", "/usr",
    "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib64", "/lib64",
    "--symlink", "usr/lib", "/lib", "--ro-bind", "/etc", "/etc",
    "--ro-bind", str(binary), "/opt/claude", "--dev", "/dev", "--proc", "/proc",
    "--tmpfs", "/tmp", "--dir", "/run", "--bind", str(root), str(root),
    "--chdir", str(work), "--setenv", "HOME", str(root),
    "--setenv", "CLAUDE_CONFIG_DIR", str(config),
    "--setenv", "ANTHROPIC_API_KEY", "test-no-network",
    "--setenv", "TERM", "xterm-256color", "--setenv", "PATH", "/usr/bin",
    "--", "/opt/claude", "--bare", "--strict-mcp-config", "--tools", "", "--resume", session_id,
]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(args[0], args)
output = b""
try:
    deadline = time.monotonic() + 30
    dismissed_key_prompt = False
    while time.monotonic() < deadline:
        if select.select([fd], [], [], 0.1)[0]:
            try:
                output += os.read(fd, 100000)
            except OSError as error:
                raise RuntimeError("native Claude exited: " + repr(output[-3000:])) from error
        if b"ANTHROPIC_API_KEY" in output and not dismissed_key_prompt:
            os.write(fd, b"\r")
            dismissed_key_prompt = True
        registry = config / "sessions" / "2.json"
        if registry.exists() and b"ready" in output:
            record = json.loads(registry.read_text())
            assert record["sessionId"] == session_id
            print(json.dumps({"supervisor_pid": pid, "session_id": session_id,
                              "projects": str(config / "projects"), "cwd": str(work)}), flush=True)
            # The Rust parent owns the test lifetime. EOF or a line requests cleanup.
            select.select([sys.stdin], [], [], 30)
            break
    else:
        raise RuntimeError("native Claude did not resume the offline fixture: " + repr(output[-2000:]))
finally:
    (root / "tui.log").write_bytes(output)
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    os.waitpid(pid, 0)
    os.close(fd)
