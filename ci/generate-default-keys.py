#!/usr/bin/env python3

import argparse
import html
import json
import subprocess
from pathlib import Path

PLATFORMS = ("linux", "macos", "windows")
MODIFIER_NAMES = {
    "linux": {"CTRL": "Ctrl", "SHIFT": "Shift", "ALT": "Alt", "SUPER": "Super"},
    "macos": {"CTRL": "Ctrl", "SHIFT": "Shift", "ALT": "Opt", "SUPER": "Cmd"},
    "windows": {"CTRL": "Ctrl", "SHIFT": "Shift", "ALT": "Alt", "SUPER": "Win"},
}
MODIFIER_ORDER = {
    "linux": ("CTRL", "SHIFT", "ALT", "SUPER", "LEADER"),
    "macos": ("SUPER", "CTRL", "ALT", "SHIFT", "LEADER"),
    "windows": ("CTRL", "SHIFT", "ALT", "SUPER", "LEADER"),
}


def load_catalog(binary: Path, platform: str) -> dict:
    result = subprocess.run(
        [
            str(binary),
            "-n",
            "show-keys",
            "--json",
            "--platform",
            platform,
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    catalog = json.loads(result.stdout)
    if catalog.get("schema_version") != 1:
        raise ValueError(
            f"unsupported default command catalog schema: {catalog.get('schema_version')}"
        )
    if catalog.get("platform") != platform:
        raise ValueError(
            f"requested {platform}, but wakterm exported {catalog.get('platform')}"
        )
    if not isinstance(catalog.get("commands"), list):
        raise ValueError(f"{platform} catalog has no commands array")
    return catalog


def action_key(action: object) -> str:
    return json.dumps(action, sort_keys=True, separators=(",", ":"))


def merge_catalogs(catalogs: dict[str, dict]) -> list[dict]:
    merged: dict[str, dict] = {}
    for platform, catalog in catalogs.items():
        for command in catalog["commands"]:
            key = action_key(command["action"])
            entry = merged.setdefault(
                key,
                {
                    "action_label": command["action_label"],
                    "brief": command["brief"],
                    "bindings": {name: [] for name in PLATFORMS},
                },
            )
            for binding in command["bindings"]:
                if binding not in entry["bindings"][platform]:
                    entry["bindings"][platform].append(binding)
    return sorted(
        (entry for entry in merged.values() if any(entry["bindings"].values())),
        key=lambda entry: (entry["brief"].casefold(), entry["action_label"]),
    )


def format_binding(binding: dict, platform: str) -> str:
    names = MODIFIER_NAMES[platform]
    modifiers = set(binding["modifiers"])
    order = MODIFIER_ORDER[platform]
    ordered_modifiers = [modifier for modifier in order if modifier in modifiers]
    ordered_modifiers.extend(sorted(modifiers.difference(order)))
    parts = [names.get(modifier, modifier.title()) for modifier in ordered_modifiers]
    parts.append(binding["key"])
    return f"<code>{html.escape('+'.join(parts))}</code>"


def format_bindings(bindings: list[dict], platform: str) -> str:
    if not bindings:
        return "-"
    return "<br>".join(format_binding(binding, platform) for binding in bindings)


def render(catalogs: dict[str, dict]) -> str:
    lines = [
        "---",
        "search:",
        "  boost: 20",
        "keywords: default keys key",
        "tags:",
        " - keys",
        "---",
        "",
        "The default key assignments are shown below. This table is generated from the built-in command metadata for each platform.",
        "",
        "Use `wakterm show-keys` to inspect the bindings after loading your config. Use `wakterm show-keys --json --platform linux|macos|windows` to export the built-in command metadata as structured data.",
        "",
        "| Action | Description | Linux | macOS | Windows |",
        "| ------ | ----------- | ----- | ----- | ------- |",
    ]
    for command in merge_catalogs(catalogs):
        label = f"<code>{html.escape(command['action_label'])}</code>"
        brief = html.escape(command["brief"])
        cells = [
            label,
            brief,
            *(
                format_bindings(command["bindings"][platform], platform)
                for platform in PLATFORMS
            ),
        ]
        lines.append("| " + " | ".join(cells) + " |")

    lines.extend(
        [
            "",
            "If you do not want the default assignments to be registered, you can disable all of them with this configuration:",
            "",
            "```lua",
            "config.disable_default_key_bindings = true",
            "```",
            "",
            "When using `disable_default_key_bindings`, it is recommended that you assign [ShowDebugOverlay](../../reference/lua/keyassignment/ShowDebugOverlay.md) and [ActivateCommandPalette](../../reference/lua/keyassignment/ActivateCommandPalette.md) to custom shortcuts for troubleshooting.",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Generate the cross-platform default key reference from wakterm"
    )
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()

    catalogs = {platform: load_catalog(args.binary, platform) for platform in PLATFORMS}
    print(render(catalogs), end="")


if __name__ == "__main__":
    main()
