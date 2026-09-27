#!/usr/bin/env python3
"""
Generate a simplification review sheet for polygon-like harness icons.

This is the reproducible version of the Claude/Codex review workflow used for
tab-bar agent icons:

1. start from an official or traced SVG that is already reduced to line/path
   geometry we can reason about
2. simplify closed outlines with Ramer-Douglas-Peucker at a few tolerances
3. rasterize each candidate with ImageMagick
4. assemble a side-by-side PNG review sheet

The parser is intentionally narrow:
- absolute SVG path commands only: M, L, H, V, Z
- no cubic or quadratic curves
- no transforms

If the source art is raster-only or curve-heavy, trace/flatten it first
(for example with vtracer) and then feed the stripped SVG into this script.
"""

from __future__ import annotations

import argparse
import math
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable
from xml.etree import ElementTree as ET

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError as exc:  # pragma: no cover - runtime environment issue
    raise SystemExit(
        "Pillow is required for scripts/icon_simplify_review.py"
    ) from exc


TOKEN_RE = re.compile(r"[MLHVZmlhvz]|-?(?:\d+(?:\.\d+)?|\.\d+)(?:[eE][+-]?\d+)?")
DEFAULT_BG = "#e8e0d3"
DEFAULT_CARD = "#f6f1e8"
DEFAULT_TEXT = "#2d2923"
DEFAULT_SUBTEXT = "#6f675e"


@dataclass
class Subpath:
    points: list[tuple[float, float]]
    closed: bool


@dataclass
class Card:
    title: str
    subtitle: str
    image_path: Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Create a simplification review sheet for a polygon-like SVG icon."
    )
    parser.add_argument("--svg", required=True, help="Input SVG path file.")
    parser.add_argument(
        "--out-dir",
        required=True,
        help="Directory for intermediate SVG and PNG candidates.",
    )
    parser.add_argument(
        "--sheet",
        required=True,
        help="Output PNG review sheet path.",
    )
    parser.add_argument(
        "--candidate",
        action="append",
        default=[],
        help=(
            "Candidate in the form EPS[:TITLE]. "
            "Example: --candidate 1.0:Low --candidate 2.0:Aggressive"
        ),
    )
    parser.add_argument(
        "--compare",
        action="append",
        default=[],
        help=(
            "Extra comparison card in the form TITLE|SUBTITLE|PATH. "
            "Useful for vtracer/bootstrap outputs."
        ),
    )
    parser.add_argument(
        "--official-title",
        default="Official source",
        help="Card title for the original SVG.",
    )
    parser.add_argument(
        "--official-subtitle",
        default="Original polygon path",
        help="Card subtitle for the original SVG.",
    )
    parser.add_argument(
        "--fill",
        default="#111111",
        help="Fill color used when writing simplified SVG candidates.",
    )
    parser.add_argument(
        "--render-size",
        type=int,
        default=256,
        help="Square raster size for candidate PNGs.",
    )
    parser.add_argument(
        "--font",
        default="",
        help="Optional font path for the review sheet labels.",
    )
    return parser.parse_args()


def parse_candidate(spec: str) -> tuple[float, str | None]:
    eps_text, sep, title = spec.partition(":")
    return float(eps_text), title or None


def parse_compare(spec: str) -> Card:
    parts = spec.split("|")
    if len(parts) != 3:
        raise SystemExit(
            f"--compare expects TITLE|SUBTITLE|PATH, got: {spec!r}"
        )
    title, subtitle, path = parts
    return Card(title=title, subtitle=subtitle, image_path=Path(path))


def parse_view_box(root: ET.Element) -> str:
    view_box = root.attrib.get("viewBox")
    if view_box:
        return view_box

    width = root.attrib.get("width")
    height = root.attrib.get("height")
    if not width or not height:
        raise SystemExit("SVG is missing both viewBox and width/height.")

    width_value = float(re.match(r"-?(?:\d+(?:\.\d+)?|\.\d+)", width).group(0))
    height_value = float(re.match(r"-?(?:\d+(?:\.\d+)?|\.\d+)", height).group(0))
    return f"0 0 {width_value:g} {height_value:g}"


def iter_path_elements(root: ET.Element) -> Iterable[ET.Element]:
    for elem in root.iter():
        if elem.tag.endswith("path") and elem.attrib.get("d"):
            yield elem


def parse_path_data(d_attr: str) -> list[Subpath]:
    tokens = TOKEN_RE.findall(d_attr)
    subpaths: list[Subpath] = []
    points: list[tuple[float, float]] = []
    cmd: str | None = None
    x = 0.0
    y = 0.0

    def flush(closed: bool) -> None:
        nonlocal points
        if points:
            subpaths.append(Subpath(points=points, closed=closed))
            points = []

    i = 0
    while i < len(tokens):
        token = tokens[i]
        if token.isalpha():
            if token in {"m", "l", "h", "v", "z"}:
                raise SystemExit(
                    f"Relative SVG path commands are not supported: {token}"
                )

            if token == "M":
                flush(closed=False)
            elif token == "Z":
                flush(closed=True)
                cmd = None
                i += 1
                continue

            cmd = token
            i += 1
            continue

        if cmd is None:
            raise SystemExit(f"Unexpected numeric token without command: {token}")

        if cmd in {"M", "L"}:
            if i + 1 >= len(tokens):
                raise SystemExit(f"Incomplete coordinate pair in path: {d_attr!r}")
            x = float(tokens[i])
            y = float(tokens[i + 1])
            points.append((x, y))
            i += 2
            if cmd == "M":
                cmd = "L"
            continue

        if cmd == "H":
            x = float(tokens[i])
            points.append((x, y))
            i += 1
            continue

        if cmd == "V":
            y = float(tokens[i])
            points.append((x, y))
            i += 1
            continue

        raise SystemExit(f"Unsupported path command: {cmd}")

    flush(closed=False)
    return [subpath for subpath in subpaths if subpath.points]


def point_line_distance(
    point: tuple[float, float],
    start: tuple[float, float],
    end: tuple[float, float],
) -> float:
    if start == end:
        return math.hypot(point[0] - start[0], point[1] - start[1])

    ax, ay = start
    bx, by = end
    px, py = point
    dx = bx - ax
    dy = by - ay
    t = ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)
    t = max(0.0, min(1.0, t))
    proj_x = ax + t * dx
    proj_y = ay + t * dy
    return math.hypot(px - proj_x, py - proj_y)


def rdp(points: list[tuple[float, float]], eps: float) -> list[tuple[float, float]]:
    if len(points) <= 2:
        return points[:]

    start = points[0]
    end = points[-1]
    max_distance = -1.0
    split_index = 0
    for idx, point in enumerate(points[1:-1], start=1):
        distance = point_line_distance(point, start, end)
        if distance > max_distance:
            max_distance = distance
            split_index = idx

    if max_distance > eps:
        left = rdp(points[: split_index + 1], eps)
        right = rdp(points[split_index:], eps)
        return left[:-1] + right

    return [start, end]


def simplify_subpath(subpath: Subpath, eps: float) -> Subpath:
    if subpath.closed:
        simplified = rdp(subpath.points + [subpath.points[0]], eps)[:-1]
        return Subpath(points=simplified, closed=True)
    return Subpath(points=rdp(subpath.points, eps), closed=False)


def subpath_to_d(subpath: Subpath) -> str:
    if not subpath.points:
        return ""
    parts = [f"M {subpath.points[0][0]:.2f} {subpath.points[0][1]:.2f}"]
    for x, y in subpath.points[1:]:
        parts.append(f"L {x:.2f} {y:.2f}")
    if subpath.closed:
        parts.append("Z")
    return " ".join(parts)


def write_svg(
    output_path: Path,
    subpaths: list[Subpath],
    view_box: str,
    fill: str,
) -> None:
    svg_lines = [
        '<svg xmlns="http://www.w3.org/2000/svg"',
        f'     viewBox="{view_box}">',
    ]
    for subpath in subpaths:
        svg_lines.append(f'  <path d="{subpath_to_d(subpath)}" fill="{fill}"/>')
    svg_lines.append("</svg>")
    output_path.write_text("\n".join(svg_lines) + "\n", encoding="utf-8")


def rasterize_svg(svg_path: Path, png_path: Path, render_size: int) -> None:
    magick = shutil.which("magick")
    if not magick:
        raise SystemExit("ImageMagick 'magick' command is required.")

    # Rasterize at about twice the output size so a small viewBox is not
    # upscaled from 72 dpi, and set the background before reading the SVG so
    # the icon stays transparent.
    view_box = parse_view_box(ET.parse(svg_path).getroot()).split()
    extent = max(float(view_box[2]), float(view_box[3]))
    density = max(72.0, 72.0 * 2 * render_size / extent)
    subprocess.run(
        [
            magick,
            "-background",
            "none",
            "-density",
            f"{density:.0f}",
            str(svg_path),
            "-trim",
            "+repage",
            "-resize",
            f"{render_size}x{render_size}",
            str(png_path),
        ],
        check=True,
    )


def load_font(font_path: str, size: int) -> ImageFont.ImageFont:
    if font_path:
        return ImageFont.truetype(font_path, size)
    try:
        return ImageFont.truetype(
            "/usr/share/fonts/google-noto-sans-vf/NotoSans[wdth,wght].ttf", size
        )
    except OSError:
        return ImageFont.load_default()


def build_review_sheet(cards: list[Card], output_path: Path, font_path: str) -> None:
    columns = 2
    rows = max(1, math.ceil(len(cards) / columns))
    margin = 30
    gap = 30
    card_width = 555
    card_height = 295
    image_box_height = 220
    width = margin * 2 + columns * card_width + (columns - 1) * gap
    height = margin * 2 + rows * card_height + (rows - 1) * gap

    canvas = Image.new("RGBA", (width, height), DEFAULT_BG)
    draw = ImageDraw.Draw(canvas)
    title_font = load_font(font_path, 22)
    subtitle_font = load_font(font_path, 16)

    for index, card in enumerate(cards):
        row = index // columns
        col = index % columns
        x = margin + col * (card_width + gap)
        y = margin + row * (card_height + gap)
        draw.rounded_rectangle(
            (x, y, x + card_width, y + card_height),
            radius=18,
            fill=DEFAULT_CARD,
        )

        image = Image.open(card.image_path).convert("RGBA")
        image.thumbnail((220, 220))
        image_x = x + (card_width - image.width) // 2
        image_y = y + 30 + (image_box_height - image.height) // 2
        canvas.alpha_composite(image, (image_x, image_y))

        title_bbox = draw.textbbox((0, 0), card.title, font=title_font)
        title_width = title_bbox[2] - title_bbox[0]
        draw.text(
            (x + (card_width - title_width) // 2, y + 245),
            card.title,
            font=title_font,
            fill=DEFAULT_TEXT,
        )

        subtitle_bbox = draw.textbbox((0, 0), card.subtitle, font=subtitle_font)
        subtitle_width = subtitle_bbox[2] - subtitle_bbox[0]
        draw.text(
            (x + (card_width - subtitle_width) // 2, y + 278),
            card.subtitle,
            font=subtitle_font,
            fill=DEFAULT_SUBTEXT,
        )

    output_path.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(output_path)


def main() -> int:
    args = parse_args()
    svg_path = Path(args.svg)
    out_dir = Path(args.out_dir)
    sheet_path = Path(args.sheet)
    out_dir.mkdir(parents=True, exist_ok=True)

    tree = ET.parse(svg_path)
    root = tree.getroot()
    view_box = parse_view_box(root)

    subpaths: list[Subpath] = []
    for path_elem in iter_path_elements(root):
        subpaths.extend(parse_path_data(path_elem.attrib["d"]))

    if not subpaths:
        raise SystemExit(f"No usable <path d=...> data found in {svg_path}")

    cards: list[Card] = []

    official_svg = out_dir / "official.svg"
    official_png = out_dir / "official.png"
    write_svg(official_svg, subpaths, view_box, args.fill)
    rasterize_svg(official_svg, official_png, args.render_size)
    cards.append(
        Card(
            title=args.official_title,
            subtitle=args.official_subtitle,
            image_path=official_png,
        )
    )

    for compare_spec in args.compare:
        cards.append(parse_compare(compare_spec))

    if not args.candidate:
        raise SystemExit("At least one --candidate is required.")

    for index, candidate_spec in enumerate(args.candidate, start=1):
        eps, title = parse_candidate(candidate_spec)
        simplified = [simplify_subpath(subpath, eps) for subpath in subpaths]
        point_count = sum(len(subpath.points) for subpath in simplified)
        stem = f"candidate-{index}-{str(eps).replace('.', '_')}"
        candidate_svg = out_dir / f"{stem}.svg"
        candidate_png = out_dir / f"{stem}.png"
        write_svg(candidate_svg, simplified, view_box, args.fill)
        rasterize_svg(candidate_svg, candidate_png, args.render_size)
        cards.append(
            Card(
                title=title or f"Simplified {index}",
                subtitle=f"{point_count} points @ eps {eps:g}",
                image_path=candidate_png,
            )
        )

    build_review_sheet(cards, sheet_path, args.font)

    print(f"review sheet: {sheet_path}")
    for card in cards:
        print(f"- {card.title}: {card.image_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
