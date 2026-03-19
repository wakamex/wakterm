#!/usr/bin/env python3
"""Validate pane rectangles from `wakterm cli list --format json`.

The checker verifies that each tab's panes can be recursively decomposed into
legal wakterm split boxes separated by single-cell dividers. That catches the
broken cases we saw during mux debugging:

- offscreen panes from oversized tab roots
- overlaps
- gaps larger than a divider
- impossible 1x0 / 0xN style panes

Usage:
    python3 check-pane-layout.py
    wakterm cli list --format json | python3 check-pane-layout.py
    python3 check-pane-layout.py --file panes.json
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass
from typing import Iterable


@dataclass(frozen=True)
class Rect:
    pane_id: int
    left: int
    top: int
    cols: int
    rows: int

    @property
    def right(self) -> int:
        return self.left + self.cols

    @property
    def bottom(self) -> int:
        return self.top + self.rows

    def describe(self) -> str:
        return (
            f"pane {self.pane_id} @{self.left},{self.top} "
            f"{self.cols}x{self.rows}"
        )


def load_panes(args: argparse.Namespace) -> list[dict]:
    if args.file:
        with open(args.file) as f:
            return json.load(f)

    if not sys.stdin.isatty():
        data = sys.stdin.read()
        if data.strip():
            return json.loads(data)

    cmd = ["wakterm", "cli", "list", "--format", "json"]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
    if result.returncode != 0:
        raise SystemExit(result.stderr.strip() or "wakterm cli list failed")
    return json.loads(result.stdout)


def overlap(a: Rect, b: Rect) -> bool:
    return not (
        a.right <= b.left
        or b.right <= a.left
        or a.bottom <= b.top
        or b.bottom <= a.top
    )


def pairwise_overlaps(rects: list[Rect]) -> list[str]:
    errors = []
    for idx, rect in enumerate(rects):
        for other in rects[idx + 1 :]:
            if overlap(rect, other):
                errors.append(
                    f"overlap: {rect.describe()} intersects {other.describe()}"
                )
    return errors


def find_small_panes(
    rects: Iterable[Rect], min_cols: int, min_rows: int
) -> tuple[list[str], list[str]]:
    errors = []
    warnings = []
    for rect in rects:
        if rect.cols <= 0 or rect.rows <= 0:
            errors.append(f"degenerate size: {rect.describe()}")
            continue
        if rect.cols < min_cols or rect.rows < min_rows:
            warnings.append(
                f"small pane: {rect.describe()} (threshold {min_cols}x{min_rows})"
            )
    return errors, warnings


def validate_box(
    rects: list[Rect],
    left: int,
    top: int,
    right: int,
    bottom: int,
) -> list[str]:
    if not rects:
        return [f"empty box {left},{top} {right-left}x{bottom-top}"]

    if len(rects) == 1:
        rect = rects[0]
        if (
            rect.left == left
            and rect.top == top
            and rect.right == right
            and rect.bottom == bottom
        ):
            return []
        return [
            "leaf does not fill its box: "
            f"{rect.describe()} vs box @{left},{top} {right-left}x{bottom-top}"
        ]

    vertical_splits = sorted(
        {rect.right for rect in rects if left < rect.right < right}
    )
    for split in vertical_splits:
        left_rects = [rect for rect in rects if rect.right <= split]
        right_rects = [rect for rect in rects if rect.left >= split + 1]
        if not left_rects or not right_rects:
            continue
        if len(left_rects) + len(right_rects) != len(rects):
            continue
        if max(rect.right for rect in left_rects) != split:
            continue
        if min(rect.left for rect in right_rects) != split + 1:
            continue

        left_errors = validate_box(left_rects, left, top, split, bottom)
        if left_errors:
            continue
        right_errors = validate_box(right_rects, split + 1, top, right, bottom)
        if right_errors:
            continue
        return []

    horizontal_splits = sorted(
        {rect.bottom for rect in rects if top < rect.bottom < bottom}
    )
    for split in horizontal_splits:
        top_rects = [rect for rect in rects if rect.bottom <= split]
        bottom_rects = [rect for rect in rects if rect.top >= split + 1]
        if not top_rects or not bottom_rects:
            continue
        if len(top_rects) + len(bottom_rects) != len(rects):
            continue
        if max(rect.bottom for rect in top_rects) != split:
            continue
        if min(rect.top for rect in bottom_rects) != split + 1:
            continue

        top_errors = validate_box(top_rects, left, top, right, split)
        if top_errors:
            continue
        bottom_errors = validate_box(bottom_rects, left, split + 1, right, bottom)
        if bottom_errors:
            continue
        return []

    summary = ", ".join(
        sorted(rect.describe() for rect in rects)
    )
    return [
        "cannot decompose panes into legal split tree within box "
        f"@{left},{top} {right-left}x{bottom-top}: {summary}"
    ]


def validate_tab(
    tab_id: int,
    tab_title: str,
    panes: list[dict],
    min_cols: int,
    min_rows: int,
) -> tuple[list[str], list[str]]:
    rects = [
        Rect(
            pane_id=pane["pane_id"],
            left=pane["left_col"],
            top=pane["top_row"],
            cols=pane["size"]["cols"],
            rows=pane["size"]["rows"],
        )
        for pane in panes
    ]

    errors, warnings = find_small_panes(rects, min_cols=min_cols, min_rows=min_rows)
    errors.extend(pairwise_overlaps(rects))

    if rects:
        right = max(rect.right for rect in rects)
        bottom = max(rect.bottom for rect in rects)
        errors.extend(validate_box(rects, 0, 0, right, bottom))

    label = f"tab {tab_id}"
    if tab_title:
        label += f" ({tab_title})"

    prefix_errors = [f"{label}: {error}" for error in errors]
    prefix_warnings = [f"{label}: {warning}" for warning in warnings]
    return prefix_errors, prefix_warnings


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate wakterm pane layouts")
    parser.add_argument("--file", help="Read pane JSON from file instead of wakterm cli")
    parser.add_argument(
        "--min-cols",
        type=int,
        default=2,
        help="Warn on panes narrower than this many columns (default: 2)",
    )
    parser.add_argument(
        "--min-rows",
        type=int,
        default=2,
        help="Warn on panes shorter than this many rows (default: 2)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable JSON summary",
    )
    args = parser.parse_args()

    panes = load_panes(args)
    tabs: dict[int, list[dict]] = {}
    titles: dict[int, str] = {}
    for pane in panes:
        tab_id = pane["tab_id"]
        tabs.setdefault(tab_id, []).append(pane)
        titles.setdefault(tab_id, pane.get("tab_title", ""))

    all_errors: list[str] = []
    all_warnings: list[str] = []
    summary = []
    for tab_id in sorted(tabs):
        tab_errors, tab_warnings = validate_tab(
            tab_id,
            titles.get(tab_id, ""),
            tabs[tab_id],
            min_cols=args.min_cols,
            min_rows=args.min_rows,
        )
        all_errors.extend(tab_errors)
        all_warnings.extend(tab_warnings)
        summary.append(
            {
                "tab_id": tab_id,
                "tab_title": titles.get(tab_id, ""),
                "pane_count": len(tabs[tab_id]),
                "ok": not tab_errors,
                "warnings": tab_warnings,
                "errors": tab_errors,
            }
        )

    if args.json:
        print(
            json.dumps(
                {
                    "ok": not all_errors,
                    "warning_count": len(all_warnings),
                    "error_count": len(all_errors),
                    "tabs": summary,
                },
                indent=2,
            )
        )
        return 1 if all_errors else 0

    for item in summary:
        title = f" ({item['tab_title']})" if item["tab_title"] else ""
        status = "ok" if item["ok"] else "BAD"
        print(
            f"{status:>3}  tab {item['tab_id']}{title}: {item['pane_count']} panes"
        )

    for warning in all_warnings:
        print(f"WARN: {warning}", file=sys.stderr)
    for error in all_errors:
        print(f"ERR:  {error}", file=sys.stderr)

    if all_errors:
        print(
            f"\n{len(all_errors)} layout error(s), {len(all_warnings)} warning(s)",
            file=sys.stderr,
        )
        return 1

    print(
        f"\nAll {len(summary)} tab(s) structurally valid"
        + (
            f" ({len(all_warnings)} warning(s))"
            if all_warnings
            else ""
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
