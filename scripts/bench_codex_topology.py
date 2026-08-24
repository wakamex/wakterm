#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pexpect>=4.9", "websockets>=15"]
# ///
"""Compare per-pane and shared Codex app-server topologies."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shutil
import signal
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

import pexpect
from benchmark_process import delta, host_environment, snapshot_roots
from codex_app_server_smoke import connect, wait_for_socket


def codex_version(codex_bin: Path) -> str:
    result = subprocess.run(
        [str(codex_bin), "--version"], text=True, capture_output=True, check=True
    )
    return result.stdout.strip()


def tui_command(codex_bin: Path, socket: Path, thread_id: str) -> list[str]:
    return [
        str(codex_bin),
        "resume",
        "--remote",
        f"unix://{socket}",
        thread_id,
        "--no-alt-screen",
        "-a",
        "never",
        "-s",
        "read-only",
    ]


def metric_summary(
    samples: list[dict], first: dict, last: dict, elapsed: float
) -> dict:
    return {
        "processes": last["processes"],
        "threads_peak": max(sample["threads"] for sample in samples),
        "fds_peak": max(sample["fds"] for sample in samples),
        "pss_kib_median": statistics.median(sample["pss_kib"] for sample in samples),
        "pss_kib_peak": max(sample["pss_kib"] for sample in samples),
        "rss_kib_median": statistics.median(sample["rss_kib"] for sample in samples),
        "private_kib_median": statistics.median(
            sample["private_kib"] for sample in samples
        ),
        "swap_kib_peak": max(sample["swap_kib"] for sample in samples),
        "cpu_seconds": delta(last, first, "cpu_ns") / 1_000_000_000,
        "cpu_percent_one_core": delta(last, first, "cpu_ns") / elapsed / 10_000_000,
        "voluntary_ctx": delta(last, first, "voluntary_ctx"),
        "involuntary_ctx": delta(last, first, "involuntary_ctx"),
        "read_syscalls": delta(last, first, "read_syscalls"),
        "write_syscalls": delta(last, first, "write_syscalls"),
        "minor_faults": delta(last, first, "minor_faults"),
        "major_faults": delta(last, first, "major_faults"),
    }


async def start_server(codex_bin: Path, socket: Path, env: dict[str, str]):
    socket.parent.mkdir(parents=True, exist_ok=True)
    process = subprocess.Popen(  # noqa: ASYNC220 - benchmark controls process topology
        [str(codex_bin), "app-server", "--listen", f"unix://{socket}"],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    await wait_for_socket(socket, process)
    rpc, _ = await connect(socket)
    return process, rpc


async def run_once(args: argparse.Namespace, topology: str, run_index: int) -> dict:
    root = Path(tempfile.mkdtemp(prefix=f"codex-{topology}-bench-"))
    codex_home = root / "codex-home"
    codex_home.mkdir()
    env = os.environ.copy()
    env.update({"CODEX_HOME": str(codex_home), "NO_COLOR": "1"})
    projects = [
        root / "projects" / f"harness-{index + 1}" for index in range(args.harnesses)
    ]
    for project in projects:
        project.mkdir(parents=True)

    servers = []
    rpcs = []
    tuis = []
    started = time.monotonic()
    try:
        server_count = 1 if topology == "shared" else args.harnesses
        pending = [
            start_server(args.codex_bin, root / "sockets" / f"server-{index}.sock", env)
            for index in range(server_count)
        ]
        for process, rpc in await asyncio.gather(*pending):
            servers.append(process)
            rpcs.append(rpc)

        threads = []
        for index, project in enumerate(projects):
            rpc = rpcs[0] if topology == "shared" else rpcs[index]
            result = await rpc.call(
                "thread/start",
                {"cwd": str(project), "serviceName": "wakterm_benchmark"},
            )
            thread = result["thread"]
            await rpc.call(
                "thread/name/set",
                {"threadId": thread["id"], "name": f"benchmark-{index + 1}"},
            )
            threads.append(thread)

        for index, thread in enumerate(threads):
            socket_index = 0 if topology == "shared" else index
            command = tui_command(
                args.codex_bin,
                root / "sockets" / f"server-{socket_index}.sock",
                thread["id"],
            )
            tuis.append(
                pexpect.spawn(
                    command[0],
                    command[1:],
                    env=env,
                    encoding="utf-8",
                    timeout=30,
                    dimensions=(73, 253),
                )
            )

        await asyncio.sleep(1)
        dead = [index + 1 for index, tui in enumerate(tuis) if not tui.isalive()]
        if dead:
            raise RuntimeError(f"Codex TUIs exited during setup: {dead}")
        ready = time.monotonic()
        await asyncio.sleep(args.settle_seconds)

        server_roots = [process.pid for process in servers]
        tui_roots = [tui.pid for tui in tuis]
        all_roots = server_roots + tui_roots
        all_samples = [snapshot_roots(all_roots)]
        server_samples = [snapshot_roots(server_roots)]
        tui_samples = [snapshot_roots(tui_roots)]
        sample_started = time.monotonic()
        while time.monotonic() - sample_started < args.sample_seconds:
            await asyncio.sleep(min(args.sample_interval, args.sample_seconds))
            if any(process.poll() is not None for process in servers):
                raise RuntimeError("Codex app-server exited during sample")
            if any(not tui.isalive() for tui in tuis):
                raise RuntimeError("Codex TUI exited during sample")
            all_samples.append(snapshot_roots(all_roots))
            server_samples.append(snapshot_roots(server_roots))
            tui_samples.append(snapshot_roots(tui_roots))
        elapsed = time.monotonic() - sample_started
        return {
            "run": run_index,
            "topology": topology,
            "harnesses": args.harnesses,
            "setup_ms": (ready - started) * 1000,
            "sample_seconds": elapsed,
            "total": metric_summary(
                all_samples, all_samples[0], all_samples[-1], elapsed
            ),
            "app_servers": metric_summary(
                server_samples, server_samples[0], server_samples[-1], elapsed
            ),
            "tuis": metric_summary(
                tui_samples, tui_samples[0], tui_samples[-1], elapsed
            ),
        }
    finally:
        for rpc in rpcs:
            await rpc.close()
        for tui in tuis:
            if tui.isalive():
                tui.close(force=True)
        for process in servers:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=5)
                except (ProcessLookupError, subprocess.TimeoutExpired):
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait(timeout=5)
        shutil.rmtree(root)


def median_runs(runs: list[dict]) -> dict:
    def medians(section: str) -> dict:
        return {
            key: statistics.median(run[section][key] for run in runs)
            for key in runs[0][section]
        }

    return {
        "setup_ms": statistics.median(run["setup_ms"] for run in runs),
        "total": medians("total"),
        "app_servers": medians("app_servers"),
        "tuis": medians("tuis"),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--codex-bin", type=Path, default=Path(shutil.which("codex") or "codex")
    )
    parser.add_argument(
        "--topology", choices=["both", "shared", "per-pane"], default="both"
    )
    parser.add_argument("--harnesses", type=int, default=20)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--settle-seconds", type=float, default=10)
    parser.add_argument("--sample-seconds", type=float, default=65)
    parser.add_argument("--sample-interval", type=float, default=5)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.harnesses < 1 or args.runs < 1 or args.sample_seconds <= 0:
        parser.error("harnesses, runs, and sample duration must be positive")
    args.codex_bin = args.codex_bin.resolve()
    return args


async def main() -> None:
    args = parse_args()
    topologies = ["per-pane", "shared"] if args.topology == "both" else [args.topology]
    grouped = {topology: [] for topology in topologies}
    for run_index in range(1, args.runs + 1):
        run_order = topologies if run_index % 2 else list(reversed(topologies))
        for topology in run_order:
            run = await run_once(args, topology, run_index)
            grouped[topology].append(run)
            print(json.dumps(run, sort_keys=True), flush=True)
    result = {
        "schema": "wakterm.codex-topology-benchmark.v1",
        "harnesses": args.harnesses,
        "results": {
            topology: {"runs": runs, "median": median_runs(runs)}
            for topology, runs in grouped.items()
        },
        "environment": {
            "codex": codex_version(args.codex_bin),
            "settle_seconds": args.settle_seconds,
            "sample_seconds": args.sample_seconds,
            "sample_interval": args.sample_interval,
            **host_environment(),
        },
    }
    rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(rendered)
    print(rendered, end="")


if __name__ == "__main__":
    asyncio.run(main())
