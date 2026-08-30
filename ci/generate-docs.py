#!/usr/bin/env python3
import glob
import io
import json
import os
import re
import subprocess
import sys
from urllib.parse import urlparse


class Page(object):
    def __init__(self, title, filename, children=None, sidebar_children=True):
        self.title = title
        self.filename = filename
        self.children = children or []
        self.sidebar_children = sidebar_children

    def render(self, output, depth=0):
        indent = "  " * depth
        bullet = "- " if depth > 0 else ""
        if depth > 0:
            if len(self.children) == 0:
                output.write(f'{indent}{bullet}"{self.title}": {self.filename}\n')
            else:
                output.write(f'{indent}{bullet}"{self.title}":\n')
                if self.filename:
                    output.write(f'{indent}  {bullet}"{self.title}": {self.filename}\n')
        for kid in self.children:
            kid.render(output, depth + 1)


# autogenerate an index page from the contents of a directory
class Gen(object):
    def __init__(self, title, dirname, index=None, extract_title=False):
        self.title = title
        self.dirname = dirname
        self.index = index
        self.extract_title = extract_title

    def render(self, output, depth=0):
        names = sorted(glob.glob(f"{self.dirname}/*.md"))
        children = []
        for filename in names:
            title = os.path.basename(filename).rsplit(".", 1)[0]
            if title == "index":
                continue

            if self.extract_title:
                with open(filename, "r") as f:
                    title = f.readline().strip("#").strip()

            children.append(Page(title, filename))

        index_filename = f"{self.dirname}/index.md"
        index_page = Page(
            self.title,
            index_filename,
            children=children,
            sidebar_children=False,
        )
        self.generated_page = index_page
        index_page.render(output, depth)
        with open(f"{self.dirname}/index.md", "w") as idx:
            if self.index:
                idx.write(self.index)
                idx.write("\n\n")
            else:
                try:
                    with open(f"{self.dirname}/index.markdown", "r") as f:
                        idx.write(f.read())
                        idx.write("\n\n")
                except FileNotFoundError:
                    pass
            for page in children:
                idx.write(f"  - [{page.title}]({os.path.basename(page.filename)})\n")


def load_scheme(scheme):
    ident = re.sub(
        "[^a-z0-9_]", "_", scheme["metadata"]["name"].lower().replace("+", "plus")
    )

    if "ansi" not in scheme["colors"]:
        raise Exception(f"scheme {scheme} is missing ansi colors!!?")
    colors = scheme["colors"]["ansi"] + scheme["colors"]["brights"]

    data = {
        "name": scheme["metadata"]["name"],
        "prefix": scheme["metadata"]["prefix"],
        "ident": ident,
        "fg": scheme["colors"]["foreground"],
        "bg": scheme["colors"]["background"],
        "metadata": scheme["metadata"],
        "ansi": scheme["colors"]["ansi"],
        "brights": scheme["colors"]["brights"],
        "cursor": scheme["colors"].get(
            "cursor_border", scheme["colors"].get("cursor_bg", scheme["colors"]["foreground"])
        ),
        "selection_fg": scheme["colors"].get("selection_fg", scheme["colors"]["background"]),
        "selection_bg": scheme["colors"].get("selection_bg", scheme["colors"]["foreground"]),
    }
    data["all_colors"] = colors
    return data


def hex_luminance(color):
    color = color.lstrip("#")
    if len(color) != 6:
        return 0
    r = int(color[0:2], 16)
    g = int(color[2:4], 16)
    b = int(color[4:6], 16)
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def classify_appearance(color):
    return "light" if hex_luminance(color) > 140 else "dark"


def classify_source(metadata):
    url = metadata.get("origin_url", "")
    hostname = urlparse(url).netloc.lower()
    if "terminal.sexy" in hostname:
        return "terminal.sexy"
    if "gogh" in hostname:
        return "Gogh"
    if "base16" in hostname:
        return "base16"
    if "iterm2colorschemes" in hostname or "mbadolato" in hostname:
        return "iTerm2"
    if hostname:
        return hostname.replace("www.", "")
    return "Other"


class GenColorScheme(object):
    def __init__(self, title, dirname, index=None):
        self.title = title
        self.dirname = dirname
        self.index = index

    def render(self, output, depth=0):
        with open("colorschemes/data.json") as f:
            scheme_data = json.load(f)
        schemes = []
        for raw in scheme_data:
            scheme = load_scheme(raw)
            entry = {
                "name": scheme["name"],
                "ident": scheme["ident"],
                "prefix": scheme["prefix"],
                "appearance": classify_appearance(scheme["bg"]),
                "source": classify_source(scheme["metadata"]),
                "author": scheme["metadata"].get("author"),
                "aliases": scheme["metadata"].get("aliases", []),
                "origin_url": scheme["metadata"].get("origin_url"),
                "wakterm_version": scheme["metadata"].get("wakterm_version"),
                "fg": scheme["fg"],
                "bg": scheme["bg"],
                "cursor": scheme["cursor"],
                "selection_fg": scheme["selection_fg"],
                "selection_bg": scheme["selection_bg"],
                "ansi": scheme["ansi"],
                "brights": scheme["brights"],
            }
            schemes.append(entry)

        schemes.sort(key=lambda item: item["name"].lower())

        os.makedirs(self.dirname, exist_ok=True)
        with open(f"{self.dirname}/catalog.json", "w") as catalog:
            json.dump(schemes, catalog)

        index_filename = f"{self.dirname}/index.md"
        index_page = Page(self.title, index_filename)
        self.generated_page = index_page
        index_page.render(output, depth)

        with open(index_filename, "w") as idx:
            idx.write(
                f"""---
hide:
  - toc
---

<link rel="stylesheet" href="browser.css">
<script defer src="browser.js"></script>

# Color Scheme Browser

wakterm ships with {len(schemes)} built-in color schemes.
Use the browser below to search by name, filter by source or appearance,
and inspect a full preview without loading thousands of embedded terminal players.

<div class="scheme-browser" data-scheme-browser>
  <div class="scheme-browser__controls">
    <label class="scheme-browser__field">
      <span>Search</span>
      <input type="search" placeholder="Batman, Gogh, nord..." data-scheme-search>
    </label>
    <label class="scheme-browser__field">
      <span>Source</span>
      <select data-scheme-source>
        <option value="">All sources</option>
      </select>
    </label>
    <label class="scheme-browser__field">
      <span>Appearance</span>
      <select data-scheme-appearance>
        <option value="">All themes</option>
        <option value="dark">Dark</option>
        <option value="light">Light</option>
      </select>
    </label>
    <label class="scheme-browser__field scheme-browser__field--match">
      <span>Match colors</span>
      <span class="scheme-browser__match-row">
        <input type="text" placeholder="#040506, #b6aa99, #ff9800" data-scheme-match>
        <input type="color" value="#1a1b26" data-scheme-picker title="Pick a color to match">
      </span>
    </label>
  </div>
  <p class="scheme-browser__summary" data-scheme-summary></p>
  <div class="scheme-browser__layout">
    <div class="scheme-browser__list" data-scheme-list></div>
    <div class="scheme-browser__detail" data-scheme-detail>
      <p class="scheme-browser__loading">Loading color schemes…</p>
    </div>
  </div>
</div>
"""
            )


TOC = [
    Page(
        "wakterm",
        "index.md",
        children=[
            Page("Features", "features.md"),
            Page("Agent Harness Lifecycle", "agent-lifecycle.md"),
            Page("Agent Prompt Submission", "agent-send.md"),
            Page("Agent API v1 Contract", "agent-api/v1/index.md"),
            Page("Experimental Agent Output", "agent-output.md"),
            Page("Scrollback", "scrollback.md"),
            Page("Quick Select Mode", "quickselect.md"),
            Page("Copy Mode", "copymode.md"),
            Page("Hyperlinks", "hyperlinks.md"),
            Page("Shell Integration", "shell-integration.md"),
            Page("iTerm Image Protocol", "imgcat.md"),
            Page("SSH", "ssh.md"),
            Page("Serial Ports & Arduino", "serial.md"),
            Page("Multiplexing", "multiplexing.md"),
        ],
    ),
    Page(
        "Download",
        "installation.md",
        children=[
            Page("Windows", "install/windows.md"),
            Page("macOS", "install/macos.md"),
            Page("Linux", "install/linux.md"),
            Page("FreeBSD", "install/freebsd.md"),
            Page("NetBSD", "install/netbsd.md"),
            Page("Build from source", "install/source.md"),
        ],
    ),
    Page(
        "Configuration",
        "guides/configuration/files.md",
        children=[
            Page("Colors & Appearance", "guides/configuration/appearance.md"),
            GenColorScheme("Color Schemes", "colorschemes"),
            Page("Launching Programs", "guides/configuration/launch.md"),
            Page("Fonts", "guides/configuration/fonts.md"),
            Page("Font Shaping", "guides/configuration/font-shaping.md"),
            Page("Keyboard Concepts", "guides/configuration/keyboard-concepts.md"),
            Page("Key Binding", "guides/configuration/keys.md"),
            Page("Key Tables", "guides/configuration/key-tables.md"),
            Page("Default Key Assignments", "guides/configuration/default-keys.md"),
            Page("Keyboard Encoding", "guides/configuration/key-encoding.md"),
            Page("Mouse Binding", "guides/configuration/mouse.md"),
            Page("Plugins", "guides/configuration/plugins.md"),
            Gen("Recipes", "guides/recipes", extract_title=True),
        ],
    ),
    Page(
        "Config & Lua",
        "reference/lua/index.md",
        children=[
            Gen(
                "Config Options",
                "reference/config",
            ),
            Gen(
                "module: wakterm",
                "reference/lua/wakterm",
            ),
            Gen(
                "module: wakterm.color",
                "reference/lua/wakterm.color",
            ),
            Gen(
                "module: wakterm.gui",
                "reference/lua/wakterm.gui",
            ),
            Gen(
                "module: wakterm.mux",
                "reference/lua/wakterm.mux",
            ),
            Gen(
                "module: wakterm.plugin",
                "reference/lua/wakterm.plugin",
            ),
            Gen(
                "module: wakterm.procinfo",
                "reference/lua/wakterm.procinfo",
            ),
            Gen(
                "module: wakterm.serde",
                "reference/lua/wakterm.serde",
            ),
            Gen(
                "module: wakterm.time",
                "reference/lua/wakterm.time",
            ),
            Gen(
                "module: wakterm.url",
                "reference/lua/wakterm.url",
            ),
            Gen(
                "enum: KeyAssignment",
                "reference/lua/keyassignment",
            ),
            Gen(
                "enum: CopyModeAssignment",
                "reference/lua/keyassignment/CopyMode",
            ),
            Gen("object: Color", "reference/lua/color"),
            Page("object: ExecDomain", "reference/lua/ExecDomain.md"),
            Page("object: LocalProcessInfo", "reference/lua/LocalProcessInfo.md"),
            Gen("object: MuxDomain", "reference/lua/MuxDomain"),
            Gen("object: MuxWindow", "reference/lua/mux-window"),
            Gen("object: MuxTab", "reference/lua/MuxTab"),
            Page("object: PaneInformation", "reference/lua/PaneInformation.md"),
            Page("object: TabInformation", "reference/lua/TabInformation.md"),
            Page("object: SshDomain", "reference/lua/SshDomain.md"),
            Page("object: SpawnCommand", "reference/lua/SpawnCommand.md"),
            Gen("object: Time", "reference/lua/wakterm.time/Time"),
            Page("object: TlsDomainClient", "reference/lua/TlsDomainClient.md"),
            Page("object: TlsDomainServer", "reference/lua/TlsDomainServer.md"),
            Gen(
                "object: Pane",
                "reference/lua/pane",
            ),
            Gen(
                "object: Window",
                "reference/lua/window",
            ),
            Page("object: WslDomain", "reference/lua/WslDomain.md"),
            Gen(
                "events: Gui",
                "reference/lua/gui-events",
            ),
            Gen(
                "events: Multiplexer",
                "reference/lua/mux-events",
            ),
            Gen(
                "events: Window",
                "reference/lua/window-events",
            ),
        ],
    ),
    Page(
        "CLI Reference",
        "cli/reference/index.md",
        children=[
            Page("wakterm agent", "cli/reference/agent.md"),
            Gen("wakterm cli", "cli/reference/cli"),
            Page("wakterm connect", "cli/reference/connect.md"),
            Page("wakterm imgcat", "cli/reference/imgcat.md"),
            Page("wakterm ls-fonts", "cli/reference/ls-fonts.md"),
            Page("wakterm record", "cli/reference/record.md"),
            Page("wakterm replay", "cli/reference/replay.md"),
            Page("wakterm serial", "cli/reference/serial.md"),
            Page("wakterm set-working-directory", "cli/reference/set-working-directory.md"),
            Page("wakterm show-keys", "cli/reference/show-keys.md"),
            Page("wakterm ssh", "cli/reference/ssh.md"),
            Page("wakterm start", "cli/reference/start.md"),
        ],
    ),
    Page(
        "Reference",
        None,
        children=[
            Page("Escape Sequences", "escape-sequences.md"),
            Page("What is a Terminal?", "what-is-a-terminal.md"),
        ],
    ),
    Page(
        "Get Help",
        None,
        children=[
            Page("Troubleshooting", "troubleshooting.md"),
            Page("F.A.Q.", "faq.md"),
            Page("Getting Help", "help.md"),
            Page("Contributing", "contributing.md"),
        ],
    ),
    Page("Change Log", "changelog.md"),
    Page("Sponsor", "sponsor.md"),
]

def navigation_node(node):
    if hasattr(node, "generated_page"):
        node = node.generated_page
    rendered = {
        "title": node.title,
        "source": node.filename,
        "children": [navigation_node(child) for child in node.children],
    }
    if not node.sidebar_children:
        rendered["sidebar_children"] = False
    return rendered


os.chdir("docs")
sink = io.StringIO()
for page in TOC:
    page.render(sink, depth=1)

navigation = [navigation_node(page) for page in TOC]
with open("../docs-site/navigation-source.json", "w") as output:
    json.dump(navigation, output, indent=2)
    output.write("\n")
