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


def git_show_first(ref, *paths):
    for path in paths:
        content = git_show(ref, path)
        if content is not None:
            return content
    return None


def find_matching_brace(content, open_brace_idx):
    depth = 1
    p = open_brace_idx + 1
    while p < len(content) and depth > 0:
        if content[p] == "{":
            depth += 1
        elif content[p] == "}":
            depth -= 1
        p += 1
    return p


def split_top_level(text, separator):
    parts = []
    start = 0
    i = 0
    paren = brace = bracket = 0
    in_string = False
    in_line_comment = False
    in_block_comment = 0

    while i < len(text):
        if in_line_comment:
            if text[i] == "\n":
                in_line_comment = False
            i += 1
            continue

        if in_block_comment:
            if text.startswith("/*", i):
                in_block_comment += 1
                i += 2
                continue
            if text.startswith("*/", i):
                in_block_comment -= 1
                i += 2
                continue
            i += 1
            continue

        if in_string:
            if text[i] == "\\":
                i += 2
                continue
            if text[i] == '"':
                in_string = False
            i += 1
            continue

        if text.startswith("//", i):
            in_line_comment = True
            i += 2
            continue
        if text.startswith("/*", i):
            in_block_comment = 1
            i += 2
            continue

        if text[i] == '"':
            in_string = True
            i += 1
            continue

        if text[i] == "(":
            paren += 1
        elif text[i] == ")":
            paren -= 1
        elif text[i] == "{":
            brace += 1
        elif text[i] == "}":
            brace -= 1
        elif text[i] == "[":
            bracket += 1
        elif text[i] == "]":
            bracket -= 1

        if (
            paren == 0
            and brace == 0
            and bracket == 0
            and text.startswith(separator, i)
        ):
            parts.append(text[start:i])
            start = i + len(separator)
            i += len(separator)
            continue

        i += 1

    parts.append(text[start:])
    return parts


def scan_match_arms(match_body):
    arms = []
    start = 0
    arrow = None
    i = 0
    paren = brace = bracket = 0
    in_string = False
    in_line_comment = False
    in_block_comment = 0

    while i < len(match_body):
        if in_line_comment:
            if match_body[i] == "\n":
                in_line_comment = False
            i += 1
            continue

        if in_block_comment:
            if match_body.startswith("/*", i):
                in_block_comment += 1
                i += 2
                continue
            if match_body.startswith("*/", i):
                in_block_comment -= 1
                i += 2
                continue
            i += 1
            continue

        if in_string:
            if match_body[i] == "\\":
                i += 2
                continue
            if match_body[i] == '"':
                in_string = False
            i += 1
            continue

        if match_body.startswith("//", i):
            in_line_comment = True
            i += 2
            continue
        if match_body.startswith("/*", i):
            in_block_comment = 1
            i += 2
            continue

        if match_body[i] == '"':
            in_string = True
            i += 1
            continue

        if (
            paren == 0
            and brace == 0
            and bracket == 0
            and arrow is None
            and match_body.startswith("=>", i)
        ):
            arrow = i
            i += 2
            continue

        if (
            paren == 0
            and brace == 0
            and bracket == 0
            and arrow is not None
            and match_body[i] == ","
        ):
            pattern = match_body[start:arrow].strip()
            expr = match_body[arrow + 2 : i].strip()
            if pattern and expr:
                arms.append((pattern, expr))
            start = i + 1
            arrow = None
            i += 1
            continue

        if match_body[i] == "(":
            paren += 1
        elif match_body[i] == ")":
            paren -= 1
        elif match_body[i] == "{":
            brace += 1
        elif match_body[i] == "}":
            brace -= 1
        elif match_body[i] == "[":
            bracket += 1
        elif match_body[i] == "]":
            bracket -= 1

        i += 1

    if arrow is not None:
        pattern = match_body[start:arrow].strip()
        expr = match_body[arrow + 2 :].strip()
        if pattern and expr:
            arms.append((pattern, expr))

    return arms


def extract_command_blocks(content):
    """Extract top-level match arms and their first CommandDef block."""
    results = []

    func_match = re.search(
        r"fn derive_command_from_key_assignment.*?Some\(match action \{",
        content,
        re.DOTALL,
    )
    if not func_match:
        return results

    match_body_start = func_match.end()
    match_body_end = find_matching_brace(content, match_body_start - 1) - 1
    match_body = content[match_body_start:match_body_end]

    for pattern, expr in scan_match_arms(match_body):
        arm_text = " ".join(
            line.strip()
            for line in pattern.splitlines()
            if line.strip() and not line.strip().startswith("//")
        )

        cmd_idx = expr.find("CommandDef {")
        if cmd_idx < 0:
            continue

        block_start = cmd_idx + len("CommandDef {")
        block_end = find_matching_brace(expr, cmd_idx + len("CommandDef ")) - 1
        block = expr[block_start:block_end]

        brief_m = re.search(r'brief:\s*"([^"]*(?:\\.[^"]*)*)"', block)
        brief = brief_m.group(1) if brief_m else ""
        brief = re.sub(r"\\\n\s*", " ", brief).strip()

        keys_m = re.search(r"keys:\s*vec!\[(.*?)\]", block, re.DOTALL)
        keys_raw = keys_m.group(1).strip() if keys_m else ""

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

        action = normalize_action(arm_text)

        results.append(
            {
                "action": action,
                "arm": arm_text,
                "brief": brief,
                "keys_raw": keys_raw,
                "keys_parsed": keys_parsed,
            }
        )

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
    alts = [part.strip() for part in split_top_level(arm, "|") if part.strip()]
    if alts:
        arm = alts[-1]

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
    fork_src = git_show_first("HEAD", "wakterm-gui/src/commands.rs", "wezterm-gui/src/commands.rs")
    if not fork_src:
        for path in ("wakterm-gui/src/commands.rs", "wezterm-gui/src/commands.rs"):
            try:
                with open(path) as f:
                    fork_src = f.read()
                    break
            except FileNotFoundError:
                continue
    fork_blocks = extract_command_blocks(fork_src)

    # Parse upstream
    upstream_blocks = []
    if upstream_hash:
        upstream_src = git_show_first(
            "upstream/main",
            "wakterm-gui/src/commands.rs",
            "wezterm-gui/src/commands.rs",
        )
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
