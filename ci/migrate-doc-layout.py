#!/usr/bin/env python3

import os
import re
import subprocess
from pathlib import Path, PurePosixPath


PROJECT = Path(__file__).resolve().parents[1]
DOCS = PROJECT / "docs"
ROUTE_MAP = PROJECT / "docs-site" / "route-map.tsv"

MARKDOWN_LINK_RE = re.compile(r"(?P<prefix>!?\[[^\]]*\]\()(?P<target>[^)]+)(?P<suffix>\))")
INCLUDE_RE = re.compile(r'(?P<prefix>{%\s*include(?:-markdown)?\s+")(?P<target>[^"]+)(?P<suffix>"\s*%})')
HTML_LINK_RE = re.compile(r'(?P<prefix>\b(?:href|src)=")(?P<target>[^"]+)(?P<suffix>")')


def tracked_docs() -> list[PurePosixPath]:
    output = subprocess.check_output(
        ["git", "ls-files", "docs"], cwd=PROJECT, text=True
    ).splitlines()
    return [PurePosixPath(path).relative_to("docs") for path in output]


def destination(path: PurePosixPath) -> PurePosixPath:
    value = path.as_posix()
    if value.startswith(("reference/", "guides/", "cli/reference/", "generated/")):
        return path
    if value == "agent-api/v1/README.md":
        return PurePosixPath("agent-api/v1/index.md")
    if value.startswith("config/lua/config/"):
        return PurePosixPath("reference/config") / path.relative_to("config/lua/config")
    if value.startswith("config/lua/"):
        relative = path.relative_to("config/lua")
        if relative == PurePosixPath("general.md"):
            return PurePosixPath("reference/configuration.md")
        return PurePosixPath("reference/lua") / relative
    if value.startswith("config/") and path.suffix == ".md":
        return PurePosixPath("guides/configuration") / path.relative_to("config")
    if value.startswith("recipes/"):
        return PurePosixPath("guides/recipes") / path.relative_to("recipes")
    if value.startswith("cli/"):
        relative = path.relative_to("cli")
        if relative == PurePosixPath("general.md"):
            return PurePosixPath("cli/reference/index.md")
        return PurePosixPath("cli/reference") / relative
    if value.startswith("examples/"):
        target = "generated/key-tables" if path.suffix == ".markdown" else "generated/cli-help"
        return PurePosixPath(target) / path.name
    return path


def resolve_target(source: PurePosixPath, target: str, mapping: dict[str, PurePosixPath]) -> str:
    if target.startswith(("http://", "https://", "mailto:", "#", "/", "@/", "<", "data:")):
        return target
    destination_text, separator, fragment = target.partition("#")
    old_target = PurePosixPath(os.path.normpath((source.parent / destination_text).as_posix()))
    candidate = DOCS / old_target
    if old_target.as_posix() not in mapping and not candidate.exists():
        return target
    new_source = mapping[source.as_posix()]
    new_target = mapping.get(old_target.as_posix(), old_target)
    relative = os.path.relpath(new_target.as_posix(), new_source.parent.as_posix())
    if relative == ".":
        relative = new_target.name
    return relative + ("#" + fragment if separator else "")


def rewrite(text: str, source: PurePosixPath, mapping: dict[str, PurePosixPath]) -> str:
    def replace_markdown(match: re.Match[str]) -> str:
        return match.group("prefix") + resolve_target(source, match.group("target"), mapping) + match.group("suffix")

    def replace_simple(match: re.Match[str]) -> str:
        return match.group("prefix") + resolve_target(source, match.group("target"), mapping) + match.group("suffix")

    text = MARKDOWN_LINK_RE.sub(replace_markdown, text)
    text = INCLUDE_RE.sub(replace_simple, text)
    return HTML_LINK_RE.sub(replace_simple, text)


def repair_legacy_links(text: str, source: PurePosixPath) -> tuple[str, int]:
    repaired = 0

    def replace(match: re.Match[str]) -> str:
        nonlocal repaired
        target = match.group("target")
        if target.startswith(("http://", "https://", "mailto:", "#", "/", "@/", "<", "data:")):
            return match.group(0)
        path, separator, fragment = target.partition("#")
        normalized = PurePosixPath(os.path.normpath(path)).as_posix()
        replacement: PurePosixPath | None = None
        config_reference = re.search(r"(?:^|/)config/lua/config/(?P<suffix>.+)$", normalized)
        lua_reference = re.search(r"(?:^|/)config/lua/(?P<suffix>.+)$", normalized)
        config_field = re.search(r"(?:^|/)config/(?P<suffix>[^/]+\.md)$", normalized)
        if config_reference:
            replacement = PurePosixPath("reference/config") / config_reference.group("suffix")
        elif lua_reference:
            replacement = PurePosixPath("reference/lua") / lua_reference.group("suffix")
        elif config_field:
            replacement = PurePosixPath("reference/config") / config_field.group("suffix")
        elif normalized.endswith("cli/index.md"):
            replacement = (
                PurePosixPath("cli/reference/cli/index.md")
                if source.as_posix().startswith("cli/reference/cli/")
                or source == PurePosixPath("cli/reference/index.md")
                else PurePosixPath("cli/reference/index.md")
            )
        if replacement is None:
            return match.group(0)
        relative = os.path.relpath(replacement.as_posix(), source.parent.as_posix())
        repaired += 1
        return match.group("prefix") + relative + ("#" + fragment if separator else "") + match.group("suffix")

    return MARKDOWN_LINK_RE.sub(replace, text), repaired


def repair_current_layout_links() -> int:
    repaired = 0
    for source in tracked_docs():
        path = DOCS / source
        if path.suffix.lower() not in {".md", ".markdown"}:
            continue
        text, count = repair_legacy_links(path.read_text(), source)
        if count:
            path.write_text(text)
            repaired += count
    return repaired


def mkdocs_route(path: PurePosixPath) -> str:
    if path.name.lower() in {"index.md", "index.markdown", "readme.md", "readme.markdown"}:
        parent = path.parent.as_posix()
        return "/index.html" if parent == "." else "/" + parent + "/index.html"
    if path.suffix in {".md", ".markdown"}:
        return "/" + path.with_suffix(".html").as_posix()
    return ""


def zola_route(path: PurePosixPath) -> str:
    if path.name.lower() in {"index.md", "index.markdown", "readme.md", "readme.markdown"}:
        parent = path.parent.as_posix()
        return "/" if parent == "." else "/" + parent + "/"
    if path.suffix in {".md", ".markdown"}:
        slug = path.stem.lower().replace("_", "-")
        return "/" + (path.parent / slug).as_posix() + "/"
    return ""


def main() -> None:
    tracked = tracked_docs()
    mapping = {path.as_posix(): destination(path) for path in tracked}
    changed = {source: target for source, target in mapping.items() if source != target.as_posix()}
    if not changed:
        repaired = repair_current_layout_links()
        print(f"Documentation layout is already migrated; repaired {repaired} legacy links")
        return

    rewritten: dict[PurePosixPath, str] = {}
    for source in tracked:
        path = DOCS / source
        if path.suffix.lower() in {".md", ".markdown"}:
            rewritten[source] = rewrite(path.read_text(), source, mapping)

    for source_text, target in sorted(changed.items(), key=lambda item: len(PurePosixPath(item[0]).parts), reverse=True):
        source = DOCS / source_text
        destination_path = DOCS / target
        destination_path.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "mv", str(source), str(destination_path)], cwd=PROJECT, check=True)

    for source, text in rewritten.items():
        (DOCS / mapping[source.as_posix()]).write_text(text)

    rows = ["old_route\tnew_route\told_source\tnew_source"]
    for source in sorted(tracked, key=lambda item: item.as_posix().lower()):
        target = mapping[source.as_posix()]
        old_route = mkdocs_route(source)
        new_route = zola_route(target)
        if old_route and new_route and old_route != new_route:
            rows.append(f"{old_route}\t{new_route}\tdocs/{source}\tdocs/{target}")
    ROUTE_MAP.parent.mkdir(parents=True, exist_ok=True)
    ROUTE_MAP.write_text("\n".join(rows) + "\n")
    repaired = repair_current_layout_links()
    print(
        f"Moved {len(changed)} tracked files, recorded {len(rows) - 1} route changes, "
        f"and repaired {repaired} legacy links"
    )


if __name__ == "__main__":
    main()
