#!/usr/bin/env python3

import argparse
import csv
import html
import json
import os
import posixpath
import re
import shutil
import subprocess
import time
from pathlib import Path, PurePosixPath


PROJECT = Path(__file__).resolve().parents[1]
SOURCE = PROJECT / "docs"
SITE = PROJECT / "docs-site"
CONTENT = SITE / "content"
DATA = SITE / "data"
STATIC = SITE / "static"
OUTPUT = PROJECT / "gh_pages"

INCLUDE_RE = re.compile(r'{%\s*include\s+"([^"]+)"\s*%}')
INCLUDE_MARKDOWN_RE = re.compile(r'{%\s*include-markdown\s+"([^"]+)"\s*%}')
MARKDOWN_LINK_RE = re.compile(r"(?P<prefix>!?\[[^\]]*\]\()(?P<target>[^)]+)(?P<suffix>\))")
MERMAID_RE = re.compile(
    r"(?:{%\s*raw\s*%}\s*)?```mermaid\s*\n(?P<body>.*?)```(?:\s*{%\s*endraw\s*%})?",
    re.DOTALL,
)
FRONT_MATTER_RE = re.compile(r"\A---\n(?P<header>.*?)\n---\n", re.DOTALL)

ASSET_SUFFIXES = {
    ".css",
    ".gif",
    ".jpeg",
    ".jpg",
    ".js",
    ".json",
    ".mp4",
    ".png",
    ".svg",
    ".ttf",
    ".webm",
    ".webp",
    ".woff",
    ".woff2",
}


def run(*command: str) -> None:
    subprocess.run(command, cwd=PROJECT, check=True)


def safe_reset(path: Path) -> None:
    resolved = path.resolve()
    if resolved not in {CONTENT.resolve(), DATA.resolve(), STATIC.resolve(), OUTPUT.resolve()}:
        raise RuntimeError(f"Refusing to reset unexpected path: {resolved}")
    if path.exists():
        shutil.rmtree(path)
    path.mkdir(parents=True)


def generate_sources() -> None:
    run("python3", "ci/generate-docs.py")
    run("python3", "ci/subst-release-info.py")


def parse_navigation() -> list[dict]:
    roots = json.loads((SITE / "navigation-source.json").read_text())
    if not isinstance(roots, list) or not roots:
        raise RuntimeError("Generated documentation navigation was empty")
    return roots


def source_documents(navigation: list[dict]) -> list[Path]:
    tracked = subprocess.check_output(
        ["git", "ls-files", "docs/**/*.md", "docs/*.md"], cwd=PROJECT, text=True
    ).splitlines()
    relative_paths = {
        PurePosixPath(path).relative_to("docs").as_posix() for path in tracked
    }
    relative_paths.update(
        node["source"]
        for node, _ancestors in flatten_navigation(navigation)
        if node.get("source") and (SOURCE / node["source"]).is_file()
    )
    return sorted(SOURCE / path for path in relative_paths)


def stage_relative(source_relative: PurePosixPath) -> PurePosixPath:
    if source_relative.name.lower() in {"index.md", "readme.md"}:
        return source_relative.parent / "_index.md"
    return source_relative


def route_for(stage: PurePosixPath) -> str:
    if stage == PurePosixPath("_index.md"):
        return "/"
    if stage.name == "_index.md":
        return "/" + stage.parent.as_posix().strip("/") + "/"
    return "/" + stage.with_suffix("").as_posix().strip("/") + "/"


def theme_path(route: str) -> str:
    return route.lstrip("/")


def clean_title(value: str) -> str:
    value = re.sub(r"<[^>]+>", "", value)
    return re.sub(r"[`*_]", "", value).strip()


def extract_title(text: str, fallback: str) -> str:
    match = re.search(r"^#\s+(.+?)\s*$", text, re.MULTILINE)
    if match:
        return clean_title(match.group(1))
    return fallback.replace("_", " ").replace("-", " ").title()


def strip_front_matter(text: str) -> tuple[str, dict]:
    match = FRONT_MATTER_RE.match(text)
    if not match:
        return text, {}
    header = match.group("header")
    metadata: dict = {}
    for key in ("title", "description", "date", "updated"):
        value = re.search(rf"^{key}:\s*['\"]?(.*?)['\"]?\s*$", header, re.MULTILINE)
        if value:
            metadata[key] = value.group(1)
    metadata["hide_toc"] = bool(re.search(r"^\s*-\s+toc\s*$", header, re.MULTILINE))
    tags_match = re.search(r"^tags:\s*\n(?P<tags>(?:\s+-\s+.*\n?)*)", header, re.MULTILINE)
    if tags_match:
        metadata["tags"] = [
            item.strip().strip("'\"")
            for item in re.findall(r"^\s+-\s+(.+?)\s*$", tags_match.group("tags"), re.MULTILINE)
        ]
    return text[match.end() :], metadata


def inline_includes(text: str, source: Path) -> str:
    def replace(match: re.Match[str]) -> str:
        included = (source.parent / match.group(1)).resolve()
        if not included.is_file():
            raise RuntimeError(f"Missing include {included} referenced by {source}")
        return included.read_text()

    text = INCLUDE_RE.sub(replace, text)
    return INCLUDE_MARKDOWN_RE.sub(replace, text)


def flatten_tabs(text: str) -> str:
    output: list[str] = []
    lines = text.splitlines()
    index = 0
    while index < len(lines):
        declaration = re.match(r'^===\s+"([^"]+)"\s*$', lines[index])
        if not declaration:
            output.append(lines[index])
            index += 1
            continue
        output.extend(("", f"## {declaration.group(1)}", ""))
        index += 1
        while index < len(lines):
            line = lines[index]
            if line.startswith("    "):
                output.append(line[4:])
                index += 1
            elif not line.strip():
                output.append("")
                index += 1
            else:
                break
    return "\n".join(output) + "\n"


def convert_admonitions(text: str) -> str:
    lines = text.splitlines()
    output: list[str] = []
    index = 0
    while index < len(lines):
        match = re.match(
            r'^(?P<indent>\s*)(?:!!!|\?\?\?\+?)\s+(?P<kind>[A-Za-z]+)(?:\s+"(?P<title>[^"]+)")?\s*$',
            lines[index],
        )
        if not match:
            output.append(lines[index])
            index += 1
            continue
        indent = match.group("indent")
        body_prefix = indent + "    "
        body: list[str] = []
        index += 1
        while index < len(lines):
            line = lines[index]
            if line.startswith(body_prefix):
                body.append(line[len(body_prefix) :])
                index += 1
            elif not line.strip():
                body.append("")
                index += 1
            else:
                break
        output.append(indent + "> [!" + match.group("kind").upper() + "]")
        if match.group("title"):
            output.append(indent + "> " + match.group("title"))
        output.extend(indent + ">" + (" " + line if line else "") for line in body)
    return "\n".join(output) + "\n"


def convert_mermaid(text: str) -> str:
    def replace(match: re.Match[str]) -> str:
        diagram = html.escape(match.group("body").rstrip())
        return '{% raw %}\n<pre class="mermaid"><code>' + diagram + "</code></pre>\n{% endraw %}"

    return MERMAID_RE.sub(replace, text)


def convert_links(
    text: str,
    source_relative: PurePosixPath,
    stage_map: dict[str, PurePosixPath],
) -> str:
    def replace(match: re.Match[str]) -> str:
        target = match.group("target")
        if target.startswith(("http://", "https://", "mailto:", "#", "/", "@/", "<")):
            return match.group(0)
        destination, separator, fragment = target.partition("#")
        normalized = posixpath.normpath((source_relative.parent / destination).as_posix())
        if destination.lower().endswith((".md", ".markdown")) and normalized in stage_map:
            converted = "@/" + stage_map[normalized].as_posix()
            if separator:
                converted += "#" + fragment
            return match.group("prefix") + converted + match.group("suffix")
        return match.group(0)

    return MARKDOWN_LINK_RE.sub(replace, text)


def remove_material_markup(text: str) -> str:
    replacements = {
        ":material-tray-arrow-down:": "download",
        ":material-alert:": "warning",
        ":material-check:": "Yes",
        ":material-close:": "No",
        ":simple-apple:": "Apple",
        ":fontawesome-brands-windows:": "Windows",
        ":simple-githubsponsors:": "GitHub Sponsors",
    }
    for source, replacement in replacements.items():
        text = text.replace(source, replacement)
    text = re.sub(r":[a-z0-9-]+:[a-z0-9-]+:", "", text)
    text = re.sub(r"\s*\{\s*[.#][^}\n]+\}\s*$", "", text, flags=re.MULTILINE)
    return text


def replace_release_values(text: str, releases: dict[str, str]) -> str:
    for key, value in releases.items():
        text = re.sub(r"\{\{\s*" + re.escape(key) + r"\s*\}\}", value, text)
    return text


def flatten_navigation(nodes: list[dict], ancestors: list[dict] | None = None) -> list[tuple[dict, list[dict]]]:
    ancestors = ancestors or []
    output: list[tuple[dict, list[dict]]] = []
    for node in nodes:
        if "source" in node:
            output.append((node, ancestors))
        output.extend(flatten_navigation(node["children"], ancestors + [node]))
    return output


def navigation_metadata(
    navigation: list[dict],
    stage_map: dict[str, PurePosixPath],
) -> tuple[dict[str, dict], list[dict]]:
    flattened = [entry for entry in flatten_navigation(navigation) if entry[0].get("source") in stage_map]
    metadata: dict[str, dict] = {}
    for index, (node, ancestors) in enumerate(flattened):
        source = node["source"]
        crumbs = []
        for ancestor in ancestors:
            if ancestor.get("source") in stage_map:
                crumbs.append(
                    {
                        "title": ancestor["title"],
                        "path": theme_path(route_for(stage_map[ancestor["source"]])),
                    }
                )
        entry = {"title": node["title"], "breadcrumbs": crumbs}
        if index:
            previous = flattened[index - 1][0]
            entry["previous"] = {
                "title": previous["title"],
                "path": theme_path(route_for(stage_map[previous["source"]])),
            }
        if index + 1 < len(flattened):
            following = flattened[index + 1][0]
            entry["next"] = {
                "title": following["title"],
                "path": theme_path(route_for(stage_map[following["source"]])),
            }
        metadata[source] = entry

    def render_node(node: dict) -> dict | None:
        rendered_children = [child for child in (render_node(item) for item in node["children"]) if child]
        rendered: dict = {"title": node["title"]}
        if node.get("source") in stage_map:
            rendered["path"] = theme_path(route_for(stage_map[node["source"]]))
        elif rendered_children:
            rendered["path"] = rendered_children[0]["path"]
        else:
            return None
        if rendered_children:
            rendered["children"] = rendered_children
        return rendered

    theme_navigation = []
    for root in navigation:
        items = [item for item in (render_node(child) for child in root["children"]) if item]
        if items:
            group = {"title": root["title"], "items": items}
            if root.get("source") in stage_map:
                group["path"] = theme_path(route_for(stage_map[root["source"]]))
            theme_navigation.append(group)
    return metadata, theme_navigation


def toml_value(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def front_matter(
    title: str,
    metadata: dict,
    navigation: dict,
    source_relative: str,
    is_section: bool,
) -> str:
    lines = ["+++", f"title = {toml_value(title)}"]
    for key in ("description", "date", "updated"):
        if metadata.get(key):
            lines.append(f"{key} = {toml_value(metadata[key])}")
    if is_section:
        lines.append('sort_by = "none"')
    lines.append("")
    lines.append("[extra]")
    lines.append(f"source_path = {toml_value('docs/' + source_relative)}")
    if metadata.get("hide_toc"):
        lines.append("toc = false")
    if navigation.get("breadcrumbs"):
        crumbs = ", ".join(
            "{ title = " + toml_value(item["title"]) + ", path = " + toml_value(item["path"]) + " }"
            for item in navigation["breadcrumbs"]
        )
        lines.append(f"breadcrumbs = [{crumbs}]")
    for direction in ("previous", "next"):
        if navigation.get(direction):
            item = navigation[direction]
            lines.append(
                direction
                + " = { title = "
                + toml_value(item["title"])
                + ", path = "
                + toml_value(item["path"])
                + " }"
            )
    lines.extend(("+++", ""))
    return "\n".join(lines)


def transform_document(
    source: Path,
    source_relative: PurePosixPath,
    stage: PurePosixPath,
    stage_map: dict[str, PurePosixPath],
    releases: dict[str, str],
    navigation: dict,
) -> str:
    text, metadata = strip_front_matter(source.read_text())
    text = inline_includes(text, source)
    text = flatten_tabs(text)
    text = convert_admonitions(text)
    text = convert_mermaid(text)
    text = convert_links(text, source_relative, stage_map)
    text = replace_release_values(text, releases)
    text = remove_material_markup(text)
    title = navigation.get("title") or metadata.get("title") or extract_title(text, stage.stem)
    return front_matter(
        title,
        metadata,
        navigation,
        source_relative.as_posix(),
        stage.name == "_index.md",
    ) + text


def write_content(navigation: list[dict]) -> tuple[int, int]:
    sources = source_documents(navigation)
    stage_map = {
        PurePosixPath(path.relative_to(SOURCE).as_posix()).as_posix(): stage_relative(
            PurePosixPath(path.relative_to(SOURCE).as_posix())
        )
        for path in sources
    }
    nav_metadata, theme_navigation = navigation_metadata(navigation, stage_map)
    releases_path = SOURCE / "releases.json"
    releases = json.loads(releases_path.read_text()) if releases_path.is_file() else {}
    mermaid_count = 0
    for source in sources:
        relative = PurePosixPath(source.relative_to(SOURCE).as_posix())
        stage = stage_map[relative.as_posix()]
        rendered = transform_document(
            source,
            relative,
            stage,
            stage_map,
            releases,
            nav_metadata.get(relative.as_posix(), {}),
        )
        destination = CONTENT / stage
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(rendered)
        mermaid_count += rendered.count('class="mermaid"')

    for directory in sorted({path.parent for path in stage_map.values()}):
        if directory == PurePosixPath("."):
            continue
        section = CONTENT / directory / "_index.md"
        if not section.exists():
            title = directory.name.replace("-", " ").replace("_", " ").title()
            section.parent.mkdir(parents=True, exist_ok=True)
            section.write_text(f"+++\ntitle = {toml_value(title)}\nsort_by = \"none\"\n+++\n")

    DATA.mkdir(parents=True, exist_ok=True)
    (DATA / "navigation.json").write_text(json.dumps(theme_navigation, indent=2) + "\n")
    return len(sources), mermaid_count


def copy_assets() -> int:
    count = 0
    for source in SOURCE.rglob("*"):
        if not source.is_file() or source.suffix.lower() not in ASSET_SUFFIXES:
            continue
        relative = source.relative_to(SOURCE)
        if relative.parts[0] == "overrides" or relative.name in {"releases.json", "catalog.json"}:
            continue
        destination = CONTENT / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
        count += 1
    shutil.copy2(PROJECT / "assets/icon/terminal.png", STATIC / "favicon.png")
    shutil.copy2(PROJECT / "assets/icon/wakterm-icon.svg", STATIC / "favicon.svg")
    fonts = STATIC / "fonts"
    fonts.mkdir()
    shutil.copy2(PROJECT / "assets/fonts/SymbolsNerdFontMono-Regular.ttf", fonts)
    shutil.copy2(SOURCE / "style.css", STATIC / "wakterm.css")
    redirects = []
    with (SITE / "legacy-redirects.tsv").open() as source:
        for row in csv.DictReader(source, delimiter="\t"):
            redirects.append(f"{row['old_route']} {row['new_route']} 301")
    (STATIC / "_redirects").write_text("\n".join(redirects) + "\n")
    return count


def output_manifest() -> dict:
    entries = []
    for path in sorted(OUTPUT.rglob("*")):
        if path.is_file():
            relative = path.relative_to(OUTPUT).as_posix()
            entries.append(
                {
                    "kind": "route" if path.suffix == ".html" else "asset",
                    "path": relative,
                    "bytes": path.stat().st_size,
                }
            )
    manifest = {"entries": entries}
    (SITE / "publication.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--zola", type=Path, default=Path(os.environ.get("ZOLA_BIN", "zola")))
    parser.add_argument("--prepare-only", action="store_true")
    args = parser.parse_args()

    started = time.perf_counter()
    for path in (CONTENT, DATA, STATIC):
        safe_reset(path)
    generate_sources()
    navigation = parse_navigation()
    document_count, mermaid_count = write_content(navigation)
    asset_count = copy_assets()
    prepare_seconds = time.perf_counter() - started
    print(
        f"Prepared {document_count} documents, {asset_count} content assets, and "
        f"{mermaid_count} Mermaid diagrams in {prepare_seconds:.3f}s"
    )
    if args.prepare_only:
        return
    if not args.zola.is_file() and shutil.which(str(args.zola)) is None:
        raise SystemExit(f"Zola binary not found: {args.zola}")
    build_started = time.perf_counter()
    run(str(args.zola), "--root", "docs-site", "build")
    build_seconds = time.perf_counter() - build_started
    check_started = time.perf_counter()
    run(str(args.zola), "--root", "docs-site", "check")
    check_seconds = time.perf_counter() - check_started
    manifest = output_manifest()
    total_bytes = sum(entry["bytes"] for entry in manifest["entries"])
    routes = sum(entry["kind"] == "route" for entry in manifest["entries"])
    print(f"Zola build: {build_seconds:.3f}s")
    print(f"Zola check: {check_seconds:.3f}s")
    print(f"Output: {len(manifest['entries'])} files, {routes} HTML routes, {total_bytes} bytes")


if __name__ == "__main__":
    main()
