"""Linux process-forest metrics shared by repeatable benchmarks."""

from __future__ import annotations

import os
import platform
from pathlib import Path

PAGE_KIB = os.sysconf("SC_PAGE_SIZE") // 1024


def host_environment() -> dict:
    cpu_model = None
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu_model = line.partition(":")[2].strip()
                break
    except OSError:
        pass
    memory_total_kib = None
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                memory_total_kib = int(line.split()[1])
                break
    except OSError:
        pass
    return {
        "kernel": platform.release(),
        "machine": platform.machine(),
        "cpu_count": os.cpu_count(),
        "cpu_model": cpu_model,
        "memory_total_kib": memory_total_kib,
    }


def proc_stat(pid: int) -> dict | None:
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
    except (FileNotFoundError, ProcessLookupError):
        return None
    close = raw.rfind(")")
    fields = raw[close + 2 :].split()
    return {
        "ppid": int(fields[1]),
        "minor_faults": int(fields[7]),
        "major_faults": int(fields[9]),
        "rss_kib": int(fields[21]) * PAGE_KIB,
    }


def cpu_nanoseconds(pid: int) -> int:
    total = 0
    try:
        tasks = Path(f"/proc/{pid}/task").iterdir()
        for task in tasks:
            try:
                total += int((task / "schedstat").read_text().split()[0])
            except (FileNotFoundError, ProcessLookupError):
                continue
    except (FileNotFoundError, ProcessLookupError):
        pass
    return total


def process_forest(root_pids: list[int]) -> list[int]:
    parents: dict[int, int] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        stat = proc_stat(int(entry.name))
        if stat is not None:
            parents[int(entry.name)] = stat["ppid"]
    selected = set(root_pids)
    changed = True
    while changed:
        changed = False
        for pid, parent in parents.items():
            if parent in selected and pid not in selected:
                selected.add(pid)
                changed = True
    return sorted(pid for pid in selected if pid in parents)


def status_file_values(path: Path) -> dict:
    values: dict[str, int] = {}
    try:
        lines = path.read_text().splitlines()
    except (FileNotFoundError, ProcessLookupError):
        return values
    wanted = {
        "Threads": "threads",
        "voluntary_ctxt_switches": "voluntary_ctx",
        "nonvoluntary_ctxt_switches": "involuntary_ctx",
    }
    for line in lines:
        key, _, raw = line.partition(":")
        if key in wanted:
            values[wanted[key]] = int(raw.strip().split()[0])
    return values


def status_values(pid: int) -> dict:
    values = status_file_values(Path(f"/proc/{pid}/status"))
    voluntary_ctx = 0
    involuntary_ctx = 0
    try:
        tasks = list(Path(f"/proc/{pid}/task").iterdir())
    except (FileNotFoundError, ProcessLookupError):
        tasks = []
    for task in tasks:
        task_values = status_file_values(task / "status")
        voluntary_ctx += task_values.get("voluntary_ctx", 0)
        involuntary_ctx += task_values.get("involuntary_ctx", 0)
    values["voluntary_ctx"] = voluntary_ctx
    values["involuntary_ctx"] = involuntary_ctx
    values["threads"] = len(tasks)
    return values


def smaps_values(pid: int) -> dict:
    values = {"pss_kib": 0, "private_kib": 0, "swap_kib": 0}
    try:
        lines = Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return values
    for line in lines:
        key, _, raw = line.partition(":")
        amount = int(raw.strip().split()[0]) if raw.strip() else 0
        if key == "Pss":
            values["pss_kib"] = amount
        elif key in {"Private_Clean", "Private_Dirty"}:
            values["private_kib"] += amount
        elif key == "SwapPss":
            values["swap_kib"] = amount
    return values


def io_values(pid: int) -> dict:
    values = {"read_syscalls": 0, "write_syscalls": 0}
    try:
        lines = Path(f"/proc/{pid}/io").read_text().splitlines()
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return values
    names = {"syscr": "read_syscalls", "syscw": "write_syscalls"}
    for line in lines:
        key, _, raw = line.partition(":")
        if key in names:
            values[names[key]] = int(raw.strip())
    return values


def snapshot_roots(root_pids: list[int]) -> dict:
    pids = process_forest(root_pids)
    totals = {
        "processes": len(pids),
        "threads": 0,
        "fds": 0,
        "cpu_ns": 0,
        "minor_faults": 0,
        "major_faults": 0,
        "rss_kib": 0,
        "pss_kib": 0,
        "private_kib": 0,
        "swap_kib": 0,
        "voluntary_ctx": 0,
        "involuntary_ctx": 0,
        "read_syscalls": 0,
        "write_syscalls": 0,
    }
    for pid in pids:
        stat = proc_stat(pid)
        if stat is None:
            continue
        for key in ["minor_faults", "major_faults", "rss_kib"]:
            totals[key] += stat[key]
        totals["cpu_ns"] += cpu_nanoseconds(pid)
        for key, value in smaps_values(pid).items():
            totals[key] += value
        status = status_values(pid)
        totals["threads"] += status.get("threads", 0)
        totals["voluntary_ctx"] += status.get("voluntary_ctx", 0)
        totals["involuntary_ctx"] += status.get("involuntary_ctx", 0)
        try:
            totals["fds"] += len(list(Path(f"/proc/{pid}/fd").iterdir()))
        except (FileNotFoundError, PermissionError):
            pass
        process_io = io_values(pid)
        totals["read_syscalls"] += process_io["read_syscalls"]
        totals["write_syscalls"] += process_io["write_syscalls"]
    root_pid = root_pids[0]
    root_stat = proc_stat(root_pid) or {}
    root_smaps = smaps_values(root_pid)
    root_status = status_values(root_pid)
    root_io = io_values(root_pid)
    try:
        root_fds = len(list(Path(f"/proc/{root_pid}/fd").iterdir()))
    except (FileNotFoundError, PermissionError):
        root_fds = 0
    totals["root"] = {
        "cpu_ns": cpu_nanoseconds(root_pid),
        "minor_faults": root_stat.get("minor_faults", 0),
        "major_faults": root_stat.get("major_faults", 0),
        "voluntary_ctx": root_status.get("voluntary_ctx", 0),
        "involuntary_ctx": root_status.get("involuntary_ctx", 0),
        **root_io,
        "rss_kib": root_stat.get("rss_kib", 0),
        **root_smaps,
        "threads": root_status.get("threads", 0),
        "fds": root_fds,
    }
    return totals


def snapshot(root_pid: int) -> dict:
    return snapshot_roots([root_pid])


def delta(after: dict, before: dict, key: str) -> int:
    return max(0, int(after[key]) - int(before[key]))
