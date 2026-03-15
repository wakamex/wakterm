#!/usr/bin/env python3
"""Generate HOTKEYS.md from wezterm show-keys output.

Uses `target/release/wezterm show-keys` for the fork's binding list,
and compares against the system-installed `wezterm` as upstream reference.

If no system wezterm is installed, upstream comparison is skipped.
The system binary typically comes from the nightly package channel
and represents the closest-to-upstream baseline.

Usage:
    python3 generate-hotkeys.py > HOTKEYS.md

Requires: target/release/wezterm binary built from current source.
"""
import re
import subprocess
import sys
from collections import defaultdict


def get_show_keys(binary="target/release/wezterm"):
    """Run wezterm show-keys and parse the text output."""
    try:
        output = subprocess.check_output(
            [binary, "show-keys"], stderr=subprocess.DEVNULL
        ).decode()
    except (subprocess.CalledProcessError, FileNotFoundError):
        print(f"Error: could not run {binary} show-keys", file=sys.stderr)
        sys.exit(1)

    bindings = []
    table = "default"

    for line in output.split("\n"):
        line = line.rstrip()
        if not line or line.startswith("-"):
            continue
        if line.endswith("key table"):
            table = line.replace(" key table", "").strip().lower()
            continue

        # Parse: <mods> <key> -> <action>
        m = re.match(r"\t([\w| ]*?)\s{2,}(\S+)\s+-> +(.+)", line)
        if m:
            mods_raw = m.group(1).strip()
            key = m.group(2)
            action = m.group(3)
            bindings.append({
                "table": table,
                "mods": mods_raw,
                "key": key,
                "action": action,
            })

    return bindings


def get_upstream_show_keys():
    """Try to get upstream bindings from the old binary or source."""
    # Try the system-installed wezterm
    for binary in ["/usr/bin/wezterm", "wezterm"]:
        try:
            output = subprocess.check_output(
                [binary, "show-keys"], stderr=subprocess.DEVNULL
            ).decode()
            version = subprocess.check_output(
                [binary, "--version"], stderr=subprocess.DEVNULL
            ).decode().strip()
            return output, version
        except (subprocess.CalledProcessError, FileNotFoundError):
            continue
    return None, None


def format_binding(mods, key):
    """Format a binding for display."""
    mod_map = {
        "CTRL": "Ctrl",
        "SHIFT": "Shift",
        "ALT": "Alt",
        "SUPER": "Super",
    }
    parts = [m.strip() for m in mods.split("|") if m.strip()]
    formatted = [mod_map.get(m, m) for m in parts]
    if formatted:
        return "+".join(formatted) + "+" + key
    return key


def action_base_name(action):
    """Extract the base action name: 'ActivateTab(3)' → 'ActivateTab'."""
    m = re.match(r"(\w+)", action)
    return m.group(1) if m else action


def main():
    bindings = get_show_keys()

    # Group by action for the default table
    default_bindings = [b for b in bindings if b["table"] == "default"]

    # Group by action base name, collecting all bindings
    by_action = defaultdict(list)
    for b in default_bindings:
        by_action[b["action"]].append(format_binding(b["mods"], b["key"]))

    # Also group by base name for summary
    by_base = defaultdict(set)
    for action, keys in by_action.items():
        base = action_base_name(action)
        by_base[base].add(action)

    # Get all KeyAssignment variants from source for completeness
    try:
        with open("config/src/keyassignment.rs") as f:
            content = f.read()
        m = re.search(r"pub enum KeyAssignment \{(.+?)\n\}", content, re.DOTALL)
        all_variants = set()
        if m:
            for line in m.group(1).split("\n"):
                vm = re.match(r"\s*(\w+)", line.strip())
                if vm and vm.group(1)[0].isupper():
                    all_variants.add(vm.group(1))
    except FileNotFoundError:
        all_variants = set()

    # Check upstream
    upstream_output, upstream_version = get_upstream_show_keys()
    upstream_actions = defaultdict(list)
    if upstream_output:
        for line in upstream_output.split("\n"):
            m = re.match(r"\t([\w| ]*?)\s{2,}(\S+)\s+-> +(.+)", line)
            if m:
                action = m.group(3)
                upstream_actions[action].append(
                    format_binding(m.group(1).strip(), m.group(2))
                )

    # Output
    print("# WezTerm Hotkeys Reference")
    print()
    print("Auto-generated from `wezterm show-keys`. "
          "Run `python3 generate-hotkeys.py > HOTKEYS.md` to update.")
    print()

    # Default key table
    print("## Default Key Bindings")
    print()

    # Deduplicate: show each unique action once with all its bindings
    # Sort by action name
    seen_actions = set()
    rows = []
    for action in sorted(by_action.keys()):
        base = action_base_name(action)
        keys = by_action[action]

        # Pick the "cleanest" binding (shortest modifier combo)
        primary = min(keys, key=len)

        # Check upstream
        up_keys = upstream_actions.get(action, [])
        if up_keys == keys:
            upstream = "same"
        elif not up_keys:
            upstream = "**new**"
        else:
            upstream = ", ".join(sorted(set(up_keys))[:2])

        # Truncate action for display
        action_display = action if len(action) <= 60 else action[:57] + "..."

        rows.append((primary, action_display, ", ".join(sorted(set(keys))[:3]), upstream))

    # Sort by primary key binding
    rows.sort(key=lambda r: r[0])

    print(f"| Key | Action | All Bindings | Upstream |")
    print(f"|-----|--------|-------------|----------|")
    for primary, action, all_keys, upstream in rows:
        action = action.replace("|", "\\|")
        print(f"| {primary} | `{action}` | {all_keys} | {upstream} |")

    print()
    print(f"*{len(rows)} bindings in the default key table.*")

    # Other key tables
    other_tables = set(b["table"] for b in bindings) - {"default"}
    for table in sorted(other_tables):
        table_bindings = [b for b in bindings if b["table"] == table]
        print()
        print(f"## {table.title()} Key Table")
        print()
        print(f"| Key | Action |")
        print(f"|-----|--------|")
        for b in sorted(table_bindings, key=lambda x: x["key"]):
            binding = format_binding(b["mods"], b["key"])
            print(f"| {binding} | `{b['action']}` |")

    # Unbound actions
    bound_bases = set(action_base_name(a) for a in by_action.keys())
    unbound = sorted(all_variants - bound_bases)
    if unbound:
        print()
        print("## Assignable Actions Without Default Bindings")
        print()
        print("These can be bound via `config.keys` in your wezterm config.")
        print()
        for v in unbound:
            print(f"- `{v}`")

    if upstream_version:
        print()
        print(f"*Upstream comparison: {upstream_version}*")


if __name__ == "__main__":
    main()
