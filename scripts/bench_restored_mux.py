#!/usr/bin/env python3
"""Benchmark a headless mux with a deterministic set of idle PTYs."""

from __future__ import annotations

import argparse
import json
import os
import signal
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

from benchmark_process import delta, host_environment, snapshot

PAYLOAD = ["/usr/bin/sleep", "infinity"]


def pane_entry(tab_index: int, pane_id: int, title: str, active: bool) -> dict:
    return {
        "window_id": 0,
        "tab_id": tab_index,
        "pane_id": pane_id,
        "agent_metadata": None,
        "title": title,
        "size": {
            "rows": 73,
            "cols": 253,
            "pixel_width": 2024,
            "pixel_height": 1314,
            "dpi": 96,
        },
        "working_dir": None,
        "is_active_pane": active,
        "is_zoomed_pane": False,
        "workspace": "benchmark",
        "cursor_pos": {
            "x": 0,
            "y": 0,
            "shape": "Default",
            "visibility": "Visible",
        },
        "physical_top": 0,
        "top_row": 0,
        "left_col": 0,
        "tty_name": None,
    }


def restored_tabs(tab_count: int, split_tabs: int, parked_tabs: int) -> list[dict]:
    tabs = []
    pane_id = 0
    for tab_index in range(tab_count):
        harness = pane_entry(
            tab_index, pane_id, f"harness-{tab_index + 1}", active=True
        )
        pane_id += 1
        tree = {"Leaf": harness}
        if tab_index < split_tabs:
            shell = pane_entry(tab_index, pane_id, "shell", active=False)
            pane_id += 1
            tree = {
                "Split": {
                    "left": {"Leaf": harness},
                    "right": {"Leaf": shell},
                    "node": {
                        "direction": "Horizontal",
                        "first": harness["size"],
                        "second": shell["size"],
                    },
                }
            }
        tabs.append(
            {
                "title": f"harness-{tab_index + 1}",
                "tree": tree,
                "parked": tab_index >= tab_count - parked_tabs,
            }
        )
    return tabs


def write_restored_session(
    data_home: Path, tab_count: int, split_tabs: int, parked_tabs: int
) -> None:
    session = {
        "version": 5,
        "windows": [
            {
                "workspace": "benchmark",
                "tabs": restored_tabs(tab_count, split_tabs, parked_tabs),
            }
        ],
        "agent_restore_intents": [],
    }
    path = data_home / "wakterm" / "session.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(session, separators=(",", ":")))


def write_config(path: Path, flavor: str, socket: Path) -> None:
    module = "wakterm" if flavor == "wakterm" else "wezterm"
    path.write_text(
        f"""local term = require '{module}'
local config = term.config_builder()
config.unix_domains = {{
  {{
    name = 'benchmark',
    socket_path = {json.dumps(str(socket))},
    connect_automatically = false,
    no_serve_automatically = true,
  }},
}}
config.default_prog = {{'/usr/bin/sleep', 'infinity'}}
config.check_for_updates = false
return config
"""
    )


def cli_prefix(flavor: str, cli_bin: Path, env: dict[str, str]) -> list[str]:
    del flavor, env
    return [str(cli_bin), "-n", "cli", "--prefer-mux", "--no-auto-start"]


def list_panes(flavor: str, cli_bin: Path, env: dict[str, str]) -> list[dict]:
    command = cli_prefix(flavor, cli_bin, env) + ["list", "--format", "json"]
    result = subprocess.run(
        command, env=env, text=True, capture_output=True, timeout=10, check=False
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or "mux list failed")
    return json.loads(result.stdout)


def wait_for_panes(
    flavor: str, cli_bin: Path, env: dict[str, str], expected: int, deadline: float
) -> list[dict]:
    last_error = "server did not become ready"
    while time.monotonic() < deadline:
        try:
            panes = list_panes(flavor, cli_bin, env)
            if len(panes) == expected:
                return panes
            last_error = f"expected {expected} panes, found {len(panes)}"
        except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
            last_error = str(error)
        time.sleep(0.025)
    raise RuntimeError(last_error)


def spawn_to_count(
    flavor: str, cli_bin: Path, env: dict[str, str], expected: int
) -> list[dict]:
    panes = wait_for_panes(flavor, cli_bin, env, 1, time.monotonic() + 15)
    pane_id = panes[0]["pane_id"]
    for _ in range(1, expected):
        command = cli_prefix(flavor, cli_bin, env) + [
            "spawn",
            "--pane-id",
            str(pane_id),
            "--",
            *PAYLOAD,
        ]
        result = subprocess.run(
            command, env=env, text=True, capture_output=True, timeout=10, check=False
        )
        if result.returncode != 0:
            raise RuntimeError(result.stderr.strip() or "mux spawn failed")
    return wait_for_panes(flavor, cli_bin, env, expected, time.monotonic() + 15)


def run_once(args: argparse.Namespace, run_index: int) -> dict:
    flavor = args.flavor
    app = "wakterm" if flavor == "wakterm" else "wezterm"
    socket_var = "WAKTERM_UNIX_SOCKET" if flavor == "wakterm" else "WEZTERM_UNIX_SOCKET"
    with tempfile.TemporaryDirectory(prefix=f"{app}-mux-bench-") as raw_temp:
        temp = Path(raw_temp)
        data_home = temp / "data"
        runtime = temp / "runtime"
        config_home = temp / "config-home"
        cache_home = temp / "cache"
        for directory in [data_home, runtime, config_home, cache_home]:
            directory.mkdir(parents=True)
        runtime.chmod(0o700)
        socket = runtime / app / "bench.sock"
        config = temp / f"{app}.lua"
        write_config(config, flavor, socket)
        if args.setup == "restore":
            if flavor != "wakterm":
                raise RuntimeError("restored-session setup is Wakterm-only")
            write_restored_session(
                data_home, args.tabs, args.split_tabs, args.parked_tabs
            )

        env = os.environ.copy()
        env.update(
            {
                "XDG_DATA_HOME": str(data_home),
                "XDG_RUNTIME_DIR": str(runtime),
                "XDG_CONFIG_HOME": str(config_home),
                "XDG_CACHE_HOME": str(cache_home),
                socket_var: str(socket),
                "NO_COLOR": "1",
            }
        )
        log_path = temp / "server.log"
        started = time.monotonic()
        with log_path.open("w+") as log:
            server = subprocess.Popen(
                [str(args.server_bin), "--config-file", str(config)],
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=log,
                start_new_session=True,
            )
            try:
                if args.setup == "restore":
                    expected_panes = args.tabs + args.split_tabs
                    panes = wait_for_panes(
                        flavor,
                        args.cli_bin,
                        env,
                        expected_panes,
                        time.monotonic() + 30,
                    )
                else:
                    panes = spawn_to_count(flavor, args.cli_bin, env, args.tabs)
                ready = time.monotonic()
                time.sleep(args.settle_seconds)
                first = snapshot(server.pid)
                samples = [first]
                sample_started = time.monotonic()
                while time.monotonic() - sample_started < args.sample_seconds:
                    time.sleep(min(1.0, args.sample_seconds))
                    if server.poll() is not None:
                        raise RuntimeError(
                            f"mux server exited with {server.returncode}"
                        )
                    samples.append(snapshot(server.pid))
                last = samples[-1]
                elapsed = time.monotonic() - sample_started
                result = {
                    "run": run_index,
                    "startup_ms": (ready - started) * 1000,
                    "panes": len(panes),
                    "tabs": len({pane["tab_id"] for pane in panes}),
                    "windows": len({pane["window_id"] for pane in panes}),
                    "sample_seconds": elapsed,
                    "tree_processes": last["processes"],
                    "tree_threads_peak": max(sample["threads"] for sample in samples),
                    "tree_fds_peak": max(sample["fds"] for sample in samples),
                    "tree_pss_kib_median": statistics.median(
                        sample["pss_kib"] for sample in samples
                    ),
                    "tree_pss_kib_peak": max(sample["pss_kib"] for sample in samples),
                    "tree_rss_kib_median": statistics.median(
                        sample["rss_kib"] for sample in samples
                    ),
                    "tree_private_kib_median": statistics.median(
                        sample["private_kib"] for sample in samples
                    ),
                    "tree_swap_kib_peak": max(sample["swap_kib"] for sample in samples),
                    "root_pss_kib_median": statistics.median(
                        sample["root"]["pss_kib"] for sample in samples
                    ),
                    "root_rss_kib_median": statistics.median(
                        sample["root"]["rss_kib"] for sample in samples
                    ),
                    "root_private_kib_median": statistics.median(
                        sample["root"]["private_kib"] for sample in samples
                    ),
                    "root_threads_peak": max(
                        sample["root"]["threads"] for sample in samples
                    ),
                    "root_fds_peak": max(sample["root"]["fds"] for sample in samples),
                    "cpu_seconds": delta(last, first, "cpu_ns") / 1_000_000_000,
                    "cpu_percent_one_core": (
                        delta(last, first, "cpu_ns") / 1_000_000_000 / elapsed * 100
                    ),
                    "root_cpu_seconds": delta(last["root"], first["root"], "cpu_ns")
                    / 1_000_000_000,
                    "root_cpu_percent_one_core": (
                        delta(last["root"], first["root"], "cpu_ns")
                        / 1_000_000_000
                        / elapsed
                        * 100
                    ),
                    "voluntary_ctx": delta(last, first, "voluntary_ctx"),
                    "involuntary_ctx": delta(last, first, "involuntary_ctx"),
                    "root_voluntary_ctx": delta(
                        last["root"], first["root"], "voluntary_ctx"
                    ),
                    "root_involuntary_ctx": delta(
                        last["root"], first["root"], "involuntary_ctx"
                    ),
                    "read_syscalls": delta(last, first, "read_syscalls"),
                    "write_syscalls": delta(last, first, "write_syscalls"),
                    "minor_faults": delta(last, first, "minor_faults"),
                    "major_faults": delta(last, first, "major_faults"),
                }
                return result
            except Exception as error:
                log.flush()
                log.seek(0)
                raise RuntimeError(f"{error}\nserver log:\n{log.read()}") from error
            finally:
                try:
                    os.killpg(server.pid, signal.SIGTERM)
                    server.wait(timeout=5)
                except (ProcessLookupError, subprocess.TimeoutExpired):
                    try:
                        os.killpg(server.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    server.wait(timeout=5)


def median_summary(runs: list[dict]) -> dict:
    keys = [
        "startup_ms",
        "tree_processes",
        "tree_threads_peak",
        "tree_fds_peak",
        "tree_pss_kib_median",
        "tree_pss_kib_peak",
        "tree_rss_kib_median",
        "tree_private_kib_median",
        "tree_swap_kib_peak",
        "root_pss_kib_median",
        "root_rss_kib_median",
        "root_private_kib_median",
        "root_threads_peak",
        "root_fds_peak",
        "cpu_seconds",
        "cpu_percent_one_core",
        "root_cpu_seconds",
        "root_cpu_percent_one_core",
        "voluntary_ctx",
        "involuntary_ctx",
        "root_voluntary_ctx",
        "root_involuntary_ctx",
        "read_syscalls",
        "write_syscalls",
        "minor_faults",
        "major_faults",
    ]
    return {key: statistics.median(run[key] for run in runs) for key in keys}


def binary_version(binary: Path) -> str:
    result = subprocess.run(
        [str(binary), "--version"], text=True, capture_output=True, check=False
    )
    return (result.stdout or result.stderr).strip()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--flavor", choices=["wakterm", "wezterm"], default="wakterm")
    parser.add_argument("--setup", choices=["restore", "spawn"], default="restore")
    parser.add_argument("--server-bin", type=Path, required=True)
    parser.add_argument("--cli-bin", type=Path, required=True)
    parser.add_argument("--tabs", type=int, default=20)
    parser.add_argument("--split-tabs", type=int, default=7)
    parser.add_argument("--parked-tabs", type=int, default=8)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--settle-seconds", type=float, default=10)
    parser.add_argument("--sample-seconds", type=float, default=65)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.tabs < 1 or args.runs < 1 or args.sample_seconds <= 0:
        parser.error("tabs, runs, and sample duration must be positive")
    if not 0 <= args.split_tabs <= args.tabs:
        parser.error("split-tabs must be between zero and tabs")
    if not 0 <= args.parked_tabs <= args.tabs:
        parser.error("parked-tabs must be between zero and tabs")
    args.server_bin = args.server_bin.resolve()
    args.cli_bin = args.cli_bin.resolve()
    return args


def main() -> None:
    args = parse_args()
    runs = []
    for index in range(1, args.runs + 1):
        run = run_once(args, index)
        runs.append(run)
        print(json.dumps(run, sort_keys=True), flush=True)
    result = {
        "schema": "wakterm.mux-benchmark.v1",
        "flavor": args.flavor,
        "setup": args.setup,
        "tabs": args.tabs,
        "split_tabs": args.split_tabs if args.setup == "restore" else 0,
        "parked_tabs": args.parked_tabs if args.setup == "restore" else 0,
        "runs": runs,
        "median": median_summary(runs),
        "environment": {
            "server": binary_version(args.server_bin),
            "payload": PAYLOAD,
            "settle_seconds": args.settle_seconds,
            "sample_seconds": args.sample_seconds,
            **host_environment(),
        },
    }
    rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(rendered)
    print(rendered, end="")


if __name__ == "__main__":
    main()
