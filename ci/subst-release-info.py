#!/usr/bin/env python3
import json
import os
import pathlib
import re

CATEGORIZE = {
    r".centos(\d+)(:?\S+)?.rpm$": "centos\\1_rpm",
    r".fedora(\d+)(:?\S+)?.rpm$": "fedora\\1_rpm",
    r".el(\d+).x86_64.rpm$": "centos\\1_rpm",
    r".fc(\d+).x86_64.rpm$": "fedora\\1_rpm",
    r".opensuse_leap(.*).rpm$": "opensuse_leap_rpm",
    r".opensuse_tumbleweed(.*).rpm$": "opensuse_tumbleweed_rpm",
    r"Debian(\d+)(\.\d+)?\.deb$": "debian\\1_deb",
    r"Ubuntu(\d+)(\.\d+)?.AppImage$": "ubuntu\\1_AppImage",
    r"Ubuntu(\d+)(\.\d+)?.deb$": "ubuntu\\1_deb",
    r"Ubuntu(\d+)(\.\d+)?\.arm64\.deb$": "ubuntu\\1_arm64_deb",
    r"Debian(\d+)(\.\d+)?\.arm64\.deb$": "debian\\1_arm64_deb",
    r"Ubuntu20.04.tar.xz$": "linux_raw_bin",
    r"^wakterm-\d+-\d+-[a-f0-9]+.tar.xz$": "linux_raw_bin",
    r"src.tar.gz$": "src",
    r"^wakterm-macos-.*.zip$": "macos_zip",
    r"^wakterm-windows-.*.zip$": "windows_zip",
    r"^wakterm-.*.setup.exe$": "windows_exe",
    r"alpine(\d+)\.(\d+)(:?-\S+)?.apk": "alpine\\1_\\2_apk",
}

RELEASES_PAGE = "https://github.com/wakamex/wakterm/releases"


def categorize(rel):
    if rel is None or "tag_name" not in rel or "assets" not in rel:
        return {}

    downloads = {}

    tag_name = "wakterm-%s" % rel["tag_name"]
    for asset in rel["assets"]:
        url = asset["browser_download_url"]
        name = asset["name"]

        for k, v in CATEGORIZE.items():
            matches = re.search(k, name)
            if matches:
                v = matches.expand(v)
                downloads[v] = (url, name, tag_name)

    return downloads


def pretty(o):
    return json.dumps(o, indent=4, sort_keys=True, separators=(",", ":"))


def build_subst(subst, stable, categorized):
    for kind, info in categorized.items():
        if info is None:
            continue
        url, name, dir = info
        kind = f"{kind}_{stable}"
        subst[kind] = url
        subst[f"{kind}_asset"] = name
        subst[f"{kind}_dir"] = dir


def fallback_subst():
    subst = {}
    docs_dir = pathlib.Path("docs")
    pattern = re.compile(r"\{\{\s*([A-Za-z0-9_]+)\s*\}\}")

    for page in docs_dir.rglob("*.md"):
        for key in pattern.findall(page.read_text()):
            if key.endswith("_asset"):
                subst[key] = "See releases page"
            elif key.endswith("_dir"):
                subst[key] = "wakterm-no-release-yet"
            else:
                subst[key] = RELEASES_PAGE

    return subst


def pick_latest_stable_release(release_info):
    for rel in release_info:
        if not isinstance(rel, dict):
            continue
        if rel.get("prerelease"):
            continue
        if "tag_name" in rel and "assets" in rel:
            return rel
    return None


def load_release_info():
    if os.environ.get("DOCS_OFFLINE") == "1":
        release_info = []
        nightly = None
        subst = fallback_subst()
        with open("docs/releases.json", "w") as output:
            json.dump(subst, output)
        return

    releases_path = pathlib.Path("/tmp/wakterm.releases.json")
    if releases_path.exists():
        with releases_path.open() as f:
            release_info = json.load(f)
    else:
        release_info = []

    nightly_path = pathlib.Path("/tmp/wakterm.nightly.json")
    if nightly_path.exists():
        with nightly_path.open() as f:
            nightly = json.load(f)
    else:
        nightly = None

    latest = pick_latest_stable_release(release_info if isinstance(release_info, list) else [])
    nightly = nightly if isinstance(nightly, dict) and "tag_name" in nightly and "assets" in nightly else None

    subst = fallback_subst()
    build_subst(subst, "stable", categorize(latest))
    build_subst(subst, "nightly", categorize(nightly))

    with open("docs/releases.json", "w") as output:
        json.dump(subst, output)


def main():
    load_release_info()


main()
