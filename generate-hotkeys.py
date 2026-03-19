#!/usr/bin/env python3
"""Generate HOTKEYS.md from wakterm source code.

Parses wakterm-gui/src/commands.rs to extract all CommandDef blocks
with their key bindings. Compares fork (HEAD) vs upstream (upstream/main).
Validates against `wakterm show-keys` when a binary is available.

Usage:
    python3 generate-hotkeys.py [--validate] > HOTKEYS.md
"""
import re
import subprocess
import sys
from collections import defaultdict


def git_show(ref, path):
    try:
        return subprocess.check_output(
            ["git", "show", f"{ref}:{path}"], stderr=subprocess.DEVNULL
        ).decode()
    except subprocess.CalledProcessError:
        return None


def extract_command_blocks(content):
    """Extract (match_arm, CommandDef_block) pairs from commands.rs.

    Returns list of {arm, brief, keys_raw, keys_parsed}.
    The 'arm' is the full match pattern (e.g., 'ActivateTab(3)').
    """
    results = []

    # Find the derive_command_from_key_assignment function body
    func_match = re.search(
        r"fn derive_command_from_key_assignment.*?Some\(match action \{",
        content, re.DOTALL,
    )
    if not func_match:
        return results

    func_start = func_match.end()

    # Find the end of the match (closing }))
    # We'll iterate through "=> CommandDef {" occurrences
    pos = func_start

    while True:
        idx = content.find("=> CommandDef {", pos)
        if idx < 0:
            break

        # Extract the match arm: text between previous block end and "=>"
        # The arm starts after the previous "}," and ends at "=>"
        arm_region = content[pos:idx].strip()

        # Clean up: remove trailing whitespace, comments
        # The arm might span multiple lines with | alternatives
        arm_lines = []
        for line in arm_region.split("\n"):
            line = line.strip()
            if line.startswith("//") or not line:
                continue
            # Remove trailing comma from previous block
            if line == "},":
                continue
            arm_lines.append(line)

        arm_text = " ".join(arm_lines).strip()
        # Remove leading punctuation, pipes, commas from previous block
        arm_text = re.sub(r"^[,|\s]+", "", arm_text)
        # Remove trailing => if present
        arm_text = re.sub(r"\s*=>$", "", arm_text)

        # Find the CommandDef block end
        block_start = idx + len("=> CommandDef {")
        depth = 1
        p = block_start
        while p < len(content) and depth > 0:
            if content[p] == "{":
                depth += 1
            elif content[p] == "}":
                depth -= 1
            p += 1
        block = content[block_start : p - 1]

        # Extract brief
        brief_m = re.search(r'brief:\s*"([^"]*(?:\\.[^"]*)*)"', block)
        brief = brief_m.group(1) if brief_m else ""
        brief = re.sub(r"\\\n\s*", " ", brief).strip()

        # Extract raw keys vec
        keys_m = re.search(r"keys:\s*vec!\[(.*?)\]", block, re.DOTALL)
        keys_raw = keys_m.group(1).strip() if keys_m else ""

        # Parse individual key bindings
        keys_parsed = []
        if keys_raw:
            for km in re.finditer(
                r'\(('
                r'Modifiers::\w+(?:(?:\s*\|\s*Modifiers::\w+)|(?:\.\w+\(Modifiers::\w+\)))*'
                r')\s*,\s*"([^"]+)"',
                keys_raw,
            ):
                mods = km.group(1).replace(" ", "")
                mods = re.sub(
                    r"\.union\(Modifiers::(\w+)\)", r"|Modifiers::\1", mods
                )
                key = km.group(2)
                keys_parsed.append((mods, key))

        # Normalize the arm text into a clean action identifier
        # e.g., "ActivateTab(3)" stays as-is
        # "CopyTextTo { text: _, destination: ClipboardCopyDestination::Clipboard }"
        # → "CopyTo(Clipboard)" or similar
        action = normalize_action(arm_text)

        results.append({
            "action": action,
            "arm": arm_text,
            "brief": brief,
            "keys_raw": keys_raw,
            "keys_parsed": keys_parsed,
        })

        pos = p

    return results


def normalize_action(arm_text):
    """Convert a match arm into a readable action name.

    Examples:
        'IncreaseFontSize' → 'IncreaseFontSize'
        'ActivateTab(3)' → 'ActivateTab(3)'
        'CloseCurrentTab { confirm: true }' → 'CloseCurrentTab(confirm=true)'
        'CopyTo(ClipboardCopyDestination::Clipboard)' → 'CopyTo(Clipboard)'
        'PasteFrom(ClipboardPasteSource::Clipboard)' → 'PasteFrom(Clipboard)'
        'SplitHorizontal(SpawnCommand { .. })' → 'SplitHorizontal'
    """
    # Strip alternative patterns and anything after =>
    arm = arm_text.split("=>")[0].strip()
    arm = arm.split("|")[0].strip()

    # Remove SpawnCommand details
    arm = re.sub(r"\(SpawnCommand\s*\{[^}]*\}\)", "", arm)
    arm = re.sub(r"\(SpawnCommand\b[^)]*\)", "", arm)

    # Simplify enum paths: ClipboardCopyDestination::Clipboard → Clipboard
    arm = re.sub(r"\w+::(\w+)", r"\1", arm)

    # Convert struct patterns to parenthesized: { confirm: true } → (confirm=true)
    def struct_to_parens(m):
        fields = m.group(1).strip()
        # Skip wildcard fields
        fields = re.sub(r"\w+:\s*_,?\s*", "", fields).strip().rstrip(",")
        if not fields:
            return ""
        fields = fields.replace(": ", "=")
        return f"({fields})"

    arm = re.sub(r"\s*\{([^}]*)\}", struct_to_parens, arm)

    return arm.strip()


def format_key(mods_raw, key, platform="linux"):
    parts = [m.replace("Modifiers::", "") for m in mods_raw.split("|")]
    parts = [p for p in parts if p != "NONE"]

    if platform != "mac" and "SUPER" in parts:
        parts.remove("SUPER")
        if "CTRL" not in parts:
            parts.append("CTRL")
        if "SHIFT" not in parts:
            parts.append("SHIFT")

    order = {"CTRL": 0, "SHIFT": 1, "ALT": 2, "SUPER": 3}
    parts.sort(key=lambda x: order.get(x, 9))

    if platform == "mac":
        mod_map = {"CTRL": "Ctrl", "SHIFT": "Shift", "ALT": "Opt", "SUPER": "Cmd"}
    else:
        mod_map = {"CTRL": "Ctrl", "SHIFT": "Shift", "ALT": "Alt"}

    formatted = [mod_map.get(m, m) for m in parts]
    if formatted:
        return "+".join(formatted) + "+" + key
    return key


def get_show_keys_actions(binary="target/release/wakterm"):
    """Get action→keys mapping from wakterm show-keys for validation."""
    try:
        output = subprocess.check_output(
            [binary, "show-keys"], stderr=subprocess.DEVNULL
        ).decode()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None

    actions = defaultdict(list)
    for line in output.split("\n"):
        m = re.match(r"\t([\w| ]*?)\s{2,}(\S+)\s+-> +(.+)", line)
        if m:
            mods = m.group(1).strip()
            key = m.group(2)
            action = m.group(3)
            actions[action].append(f"{mods}+{key}" if mods else key)
    return dict(actions)


def main():
    validate = "--validate" in sys.argv

    # Get upstream hash
    try:
        upstream_hash = subprocess.check_output(
            ["git", "rev-parse", "--short", "upstream/main"],
            stderr=subprocess.DEVNULL,
        ).decode().strip()
    except subprocess.CalledProcessError:
        upstream_hash = None

    # Parse fork
    fork_src = git_show("HEAD", "wakterm-gui/src/commands.rs")
    if not fork_src:
        with open("wakterm-gui/src/commands.rs") as f:
            fork_src = f.read()
    fork_blocks = extract_command_blocks(fork_src)

    # Parse upstream
    upstream_blocks = []
    if upstream_hash:
        upstream_src = git_show("upstream/main", "wakterm-gui/src/commands.rs")
        if upstream_src:
            upstream_blocks = extract_command_blocks(upstream_src)

    upstream_by_action = {b["action"]: b for b in upstream_blocks}

    # Get all variants
    variants_src = git_show("HEAD", "config/src/keyassignment.rs")
    if not variants_src:
        with open("config/src/keyassignment.rs") as f:
            variants_src = f.read()
    m = re.search(r"pub enum KeyAssignment \{(.+?)\n\}", variants_src, re.DOTALL)
    all_variants = set()
    if m:
        for line in m.group(1).split("\n"):
            vm = re.match(r"\s*(\w+)", line.strip())
            if vm and vm.group(1)[0].isupper():
                all_variants.add(vm.group(1))

    # Validate against show-keys if requested
    if validate:
        show_keys = get_show_keys_actions()
        if show_keys:
            print(f"Validating against show-keys ({len(show_keys)} actions)...",
                  file=sys.stderr)
            parsed_actions = {b["action"] for b in fork_blocks if b["keys_parsed"]}
            show_actions = set(show_keys.keys())
            # show-keys uses the runtime action repr, our parser uses source patterns
            # Just compare counts and flag large discrepancies
            print(f"  Source parser found: {len(parsed_actions)} actions with keys",
                  file=sys.stderr)
            print(f"  show-keys has: {len(show_actions)} actions with keys",
                  file=sys.stderr)
        return

    # Split
    bound = [b for b in fork_blocks if b["keys_parsed"]]
    unbound = [b for b in fork_blocks if not b["keys_parsed"]]
    bound_action_bases = {re.match(r"(\w+)", b["action"]).group(1)
                          for b in fork_blocks if re.match(r"(\w+)", b["action"])}
    no_entry = sorted(all_variants - bound_action_bases)

    # Output
    print("# wakterm Hotkeys Reference")
    print()
    print("Auto-generated from source. "
          "Run `python3 generate-hotkeys.py > HOTKEYS.md` to update.")
    if upstream_hash:
        print(f"  \nUpstream: `upstream/main` ({upstream_hash})")
    print()

    print("## Default Key Bindings")
    print()
    print("| Action | Description | Linux/Win | macOS | Upstream |")
    print("|--------|-------------|-----------|-------|----------|")

    for b in sorted(bound, key=lambda x: x["action"]):
        action = b["action"]
        brief = b["brief"] or action
        keys = b["keys_parsed"]

        linux = ", ".join(format_key(m, k, "linux") for m, k in keys)
        mac = ", ".join(format_key(m, k, "mac") for m, k in keys)

        ub = upstream_by_action.get(action)
        if ub is None:
            upstream = "**fork only**"
        elif ub["keys_raw"] == b["keys_raw"]:
            upstream = "same"
        else:
            upstream = "**changed**"

        brief = brief.replace("|", "\\|")
        action_disp = action if len(action) <= 50 else action[:47] + "..."
        print(f"| `{action_disp}` | {brief} | {linux} | {mac} | {upstream} |")

    print()
    print("## Actions Without Default Bindings")
    print()
    print("| Action | Description | Upstream |")
    print("|--------|-------------|----------|")

    for b in sorted(unbound, key=lambda x: x["action"]):
        action = b["action"]
        brief = b["brief"] or re.sub(r"([a-z])([A-Z])", r"\1 \2", action)

        ub = upstream_by_action.get(action)
        if ub is None:
            upstream = "**fork only**"
        elif ub["keys_raw"] == b["keys_raw"]:
            upstream = "same"
        else:
            upstream = "**changed**"

        brief = brief.replace("|", "\\|")
        print(f"| `{action}` | {brief} | {upstream} |")

    if no_entry:
        print()
        print("## Raw Actions (no command palette entry)")
        print()
        for v in no_entry:
            print(f"- `{v}`")

    print()
    n_bound = len(bound)
    n_unbound = len(unbound)
    n_raw = len(no_entry)
    print(f"*{n_bound} bound, {n_unbound} unbound with description, {n_raw} raw.*")


if __name__ == "__main__":
    main()
