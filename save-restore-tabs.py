#!/usr/bin/env python3
"""Save and restore wezterm tab layouts with running processes.

Save:   python3 save-restore-tabs.py save > tabs.json
Restore: python3 save-restore-tabs.py restore < tabs.json

The save captures:
- Tab titles
- Pane layout (positions, sizes, split structure)
- Working directories
- Running processes (claude, codex, etc.)

The restore recreates:
- Tabs with correct titles
- Split layout (horizontal/vertical)
- cd to the correct directory in each pane
- Optionally relaunches detected processes
"""
import json
import subprocess
import sys
import time


def cli(*args):
    """Run a wezterm cli command and return stdout."""
    cmd = ["wezterm", "cli"] + list(args)
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
    if r.returncode != 0:
        print(f"  warn: {' '.join(cmd)} failed: {r.stderr.strip()}", file=sys.stderr)
        return ""
    return r.stdout.strip()


def get_pane_procs(tty_name):
    """Get processes running on a pane's TTY."""
    pts = tty_name.replace("/dev/", "") if tty_name else ""
    if not pts:
        return []
    try:
        r = subprocess.run(
            ["ps", "-t", pts, "-o", "args", "--no-headers"],
            capture_output=True, text=True, timeout=3,
        )
        procs = []
        for line in r.stdout.strip().split("\n"):
            line = line.strip()
            if line:
                procs.append(line)
        return procs
    except Exception:
        return []


def infer_splits(panes):
    """Infer the split tree from pane positions.

    Returns a list of split operations needed to recreate the layout.
    Each operation is: {direction: "right"|"bottom", percent: N}
    """
    if len(panes) <= 1:
        return []

    # Sort by position
    panes = sorted(panes, key=lambda p: (p["top_row"], p["left_col"]))

    # Find the split structure by looking at positions
    splits = []
    remaining = list(panes)

    def find_splits(group, depth=0):
        if len(group) <= 1:
            return

        # Check for horizontal split (different left_col, same top_row for some)
        left_cols = sorted(set(p["left_col"] for p in group))
        top_rows = sorted(set(p["top_row"] for p in group))

        if len(left_cols) > 1:
            # Find the split point — first gap in left_col
            # Group panes by which side of the divider they're on
            split_col = left_cols[1]  # First pane on the right side
            left = [p for p in group if p["left_col"] < split_col]
            right = [p for p in group if p["left_col"] >= split_col]

            if left and right:
                total_width = max(p["left_col"] + p["cols"] for p in group)
                right_width = total_width - split_col
                pct = int(100 * right_width / total_width)
                splits.append({
                    "direction": "right",
                    "percent": pct,
                    "pane_index": len(left) - 1,  # Split from the last left pane
                    "depth": depth,
                })
                find_splits(left, depth + 1)
                find_splits(right, depth + 1)
                return

        if len(top_rows) > 1:
            split_row = top_rows[1]
            top = [p for p in group if p["top_row"] < split_row]
            bottom = [p for p in group if p["top_row"] >= split_row]

            if top and bottom:
                total_height = max(p["top_row"] + p["rows"] for p in group)
                bottom_height = total_height - split_row
                pct = int(100 * bottom_height / total_height)
                splits.append({
                    "direction": "bottom",
                    "percent": pct,
                    "pane_index": len(top) - 1,
                    "depth": depth,
                })
                find_splits(top, depth + 1)
                find_splits(bottom, depth + 1)
                return

    find_splits(remaining)
    return splits


def save():
    """Save current tab/pane state to JSON."""
    raw = cli("list", "--format", "json")
    if not raw:
        print("Error: could not list panes", file=sys.stderr)
        sys.exit(1)

    panes = json.loads(raw)
    tabs = {}

    for p in panes:
        tid = p["tab_id"]
        if tid not in tabs:
            tabs[tid] = {
                "tab_title": p["tab_title"],
                "window_id": p["window_id"],
                "workspace": p.get("workspace", "default"),
                "panes": [],
            }

        procs = get_pane_procs(p.get("tty_name", ""))
        # Filter to interesting processes (not shells)
        shells = {"-zsh", "zsh", "bash", "-bash", "fish", "sh"}
        interesting = [
            proc for proc in procs
            if proc.split("/")[-1].split()[0] not in shells
        ]

        cwd = p.get("cwd", "")
        if cwd.startswith("file://"):
            # Strip file://hostname/ prefix
            cwd = "/" + cwd.split("/", 3)[-1] if "/" in cwd[7:] else cwd

        tabs[tid]["panes"].append({
            "pane_id": p["pane_id"],
            "left_col": p["left_col"],
            "top_row": p["top_row"],
            "cols": p["size"]["cols"],
            "rows": p["size"]["rows"],
            "cwd": cwd,
            "procs": interesting,
            "title": p.get("title", ""),
            "is_active": p.get("is_active", False),
        })

    # Convert to list sorted by tab_id
    result = []
    for tid in sorted(tabs.keys()):
        tab = tabs[tid]
        tab["splits"] = infer_splits(tab["panes"])
        # Sort panes by position for deterministic output
        tab["panes"].sort(key=lambda p: (p["top_row"], p["left_col"]))
        result.append(tab)

    json.dump(result, sys.stdout, indent=2)
    print()
    print(f"Saved {len(result)} tabs, {sum(len(t['panes']) for t in result)} panes",
          file=sys.stderr)


def restore():
    """Restore tabs from saved JSON."""
    data = json.load(sys.stdin)
    print(f"Restoring {len(data)} tabs...", file=sys.stderr)

    for tab in data:
        title = tab["tab_title"]
        panes = tab["panes"]
        if not panes:
            continue

        first_pane = panes[0]
        cwd = first_pane.get("cwd", "")

        # Spawn the first tab/pane
        print(f"  Creating tab '{title}'...", file=sys.stderr)
        pane_id = cli("spawn", "--cwd", cwd)
        if not pane_id:
            print(f"    Failed to spawn tab for '{title}'", file=sys.stderr)
            continue
        pane_id = pane_id.strip()

        # Set tab title
        cli("set-tab-title", "--pane-id", pane_id, title)

        # Create additional panes via splits
        pane_ids = [pane_id]
        for i, pane in enumerate(panes[1:], 1):
            pane_cwd = pane.get("cwd", cwd)

            # Determine split direction from position relative to first pane
            if pane["left_col"] > panes[0]["left_col"] and pane["top_row"] == panes[0]["top_row"]:
                direction = "--right"
            elif pane["top_row"] > panes[0]["top_row"] and pane["left_col"] == panes[0]["left_col"]:
                direction = "--bottom"
            elif pane["left_col"] > 0:
                direction = "--right"
            else:
                direction = "--bottom"

            # Calculate percentage
            total_w = max(p["left_col"] + p["cols"] for p in panes)
            total_h = max(p["top_row"] + p["rows"] for p in panes)
            if direction == "--right":
                pct = int(100 * pane["cols"] / total_w)
            else:
                pct = int(100 * pane["rows"] / total_h)
            pct = max(10, min(90, pct))

            new_id = cli("split-pane", direction, "--percent", str(pct),
                        "--cwd", pane_cwd, "--pane-id", pane_ids[-1])
            if new_id:
                pane_ids.append(new_id.strip())
                print(f"    Split {direction} ({pct}%): pane {new_id.strip()}", file=sys.stderr)
            else:
                print(f"    Failed to split for pane {i}", file=sys.stderr)

        # cd to correct directories and relaunch processes
        for i, (pid, pane) in enumerate(zip(pane_ids, panes)):
            pane_cwd = pane.get("cwd", "")
            procs = pane.get("procs", [])

            # cd to directory
            if pane_cwd:
                cli("send-text", "--pane-id", pid, "--no-paste",
                    f"cd {pane_cwd}\r")
                time.sleep(0.1)

            # Relaunch known processes
            for proc_cmd in procs:
                binary = proc_cmd.split("/")[-1].split()[0]
                if binary in ("claude", "codex", "opencode", "gemini"):
                    print(f"    Pane {pid}: relaunching '{binary}'", file=sys.stderr)
                    cli("send-text", "--pane-id", pid, "--no-paste",
                        f"{binary}\r")
                    time.sleep(0.5)  # Give it time to start
                    break  # Only launch one agent per pane

        print(f"  Tab '{title}': {len(pane_ids)} panes created", file=sys.stderr)

    print(f"\nDone. Restored {len(data)} tabs.", file=sys.stderr)


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("save", "restore"):
        print("Usage:", file=sys.stderr)
        print("  python3 save-restore-tabs.py save > tabs.json", file=sys.stderr)
        print("  python3 save-restore-tabs.py restore < tabs.json", file=sys.stderr)
        sys.exit(1)

    if sys.argv[1] == "save":
        save()
    else:
        restore()


if __name__ == "__main__":
    main()
