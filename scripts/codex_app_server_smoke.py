#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "websockets>=15"]
# ///

import asyncio
import json
import os
import pathlib
import shutil
import signal
import subprocess
import tempfile
import time

import pexpect
import websockets


class Rpc:
    def __init__(self, websocket):
        self.websocket = websocket
        self.next_id = 1
        self.pending = {}
        self.notifications = []
        self.reader = asyncio.create_task(self._read())

    async def initialize(self):
        result = await self.call(
            "initialize",
            {
                "clientInfo": {
                    "name": "wakterm_smoke",
                    "title": "Wakterm smoke",
                    "version": "0",
                },
                "capabilities": {"optOutNotificationMethods": ["app/list/updated"]},
            },
        )
        await self.websocket.send(json.dumps({"method": "initialized", "params": {}}))
        return result

    async def _read(self):
        try:
            async for raw in self.websocket:
                message = json.loads(raw)
                if "id" in message and ("result" in message or "error" in message):
                    future = self.pending.pop(message["id"], None)
                    if future is not None:
                        if "error" in message:
                            future.set_exception(RuntimeError(json.dumps(message["error"])))
                        else:
                            future.set_result(message["result"])
                elif "method" in message:
                    self.notifications.append(message)
        except websockets.ConnectionClosed:
            pass

    async def call(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        await self.websocket.send(
            json.dumps({"id": request_id, "method": method, "params": params})
        )
        return await asyncio.wait_for(future, timeout=30)

    async def wait(self, method, predicate):
        deadline = time.monotonic() + int(os.environ.get("WAKTERM_SMOKE_TIMEOUT", "120"))
        seen = 0
        while time.monotonic() < deadline:
            while seen < len(self.notifications):
                message = self.notifications[seen]
                seen += 1
                if message.get("method") == method and predicate(message.get("params", {})):
                    return message
            await asyncio.sleep(0.05)
        raise RuntimeError(f"timed out waiting for {method}")

    async def close(self):
        self.reader.cancel()
        await asyncio.gather(self.reader, return_exceptions=True)
        await self.websocket.close()


async def connect(socket_path):
    websocket = await websockets.unix_connect(
        str(socket_path),
        uri="ws://localhost/rpc",
        compression=None,
        user_agent_header=None,
        max_size=None,
    )
    rpc = Rpc(websocket)
    initialized = await rpc.initialize()
    return rpc, initialized


async def wait_for_socket(path, process):
    for _ in range(200):
        if path.exists():
            return
        if process.poll() is not None:
            raise RuntimeError(f"app-server exited {process.returncode}")
        await asyncio.sleep(0.05)
    raise RuntimeError("app-server socket did not appear")


def tui_command(socket_path, thread_id, prompt):
    return [
        "codex",
        "resume",
        "--remote",
        f"unix://{socket_path}",
        thread_id,
        "--no-alt-screen",
        "-a",
        "never",
        "-s",
        "read-only",
        prompt,
    ]


async def main():
    root = pathlib.Path(tempfile.mkdtemp(prefix="wakterm-codex-smoke-"))
    socket_path = root / "app-server.sock"
    projects = [root / "project-a", root / "project-b"]
    for project in projects:
        project.mkdir()
    process = subprocess.Popen(  # noqa: ASYNC220 - pexpect smoke test is intentionally process based
        ["codex", "app-server", "--listen", f"unix://{socket_path}"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    children = []
    rpc = None
    probe = None
    try:
        await wait_for_socket(socket_path, process)
        rpc, initialized = await connect(socket_path)
        threads = []
        for index, project in enumerate(projects):
            result = await rpc.call(
                "thread/start",
                {"cwd": str(project), "serviceName": "wakterm_smoke"},
            )
            thread = result["thread"]
            threads.append(thread)
            await rpc.call(
                "thread/name/set",
                {"threadId": thread["id"], "name": f"wakterm-smoke-{index}"},
            )
        assert threads[0]["id"] != threads[1]["id"]

        probe, _ = await connect(socket_path)
        resumed = await probe.call("thread/resume", {"threadId": threads[0]["id"]})
        assert resumed["thread"]["id"] == threads[0]["id"]
        await probe.close()
        probe = None

        for index, thread in enumerate(threads):
            command = tui_command(
                socket_path,
                thread["id"],
                f"Reply with exactly SMOKE_{index} and nothing else.",
            )
            children.append(
                pexpect.spawn(
                    command[0],
                    command[1:],
                    encoding="utf-8",
                    timeout=120,
                    dimensions=(40, 120),
                )
            )

        started = await asyncio.gather(
            *[
                rpc.wait("turn/started", lambda params, thread=thread: params.get("threadId") == thread["id"])
                for thread in threads
            ]
        )
        completed = await asyncio.gather(
            *[
                rpc.wait(
                    "turn/completed",
                    lambda params, index=index: params.get("threadId") == threads[index]["id"]
                    and params.get("turn", {}).get("id") == started[index]["params"]["turn"]["id"],
                )
                for index in range(2)
            ]
        )

        children[0].close(force=True)
        await asyncio.sleep(0.5)
        assert children[1].isalive(), "disconnecting TUI A terminated TUI B"
        follow_up = await rpc.call(
            "turn/start",
            {
                "threadId": threads[1]["id"],
                "input": [{"type": "text", "text": "Reply with exactly STILL_B."}],
            },
        )
        follow_up_id = follow_up["turn"]["id"]
        final = await rpc.wait(
            "turn/completed",
            lambda params: params.get("threadId") == threads[1]["id"]
            and params.get("turn", {}).get("id") == follow_up_id,
        )
        assert children[1].isalive()
        print(
            json.dumps(
                {
                    "codexVersion": subprocess.check_output(  # noqa: ASYNC221
                        ["codex", "--version"], text=True
                    ).strip(),
                    "initializeUserAgent": initialized.get("userAgent"),
                    "threads": [
                        {"threadId": thread["id"], "sessionId": thread["sessionId"], "cwd": str(projects[index])}
                        for index, thread in enumerate(threads)
                    ],
                    "initialTurns": [message["params"]["turn"] for message in completed],
                    "postDisconnectTurn": final["params"]["turn"],
                    "notificationCount": len(rpc.notifications),
                },
                indent=2,
            )
        )
    finally:
        if probe is not None:
            await probe.close()
        if rpc is not None:
            await rpc.close()
        for child in children:
            if child.isalive():
                child.close(force=True)
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
        shutil.rmtree(root)


if __name__ == "__main__":
    asyncio.run(main())
