#!/usr/bin/env python3
"""Check for pane size mismatches across wezterm tabs.

Usage: wezterm cli list --format json | python3 check-pane-sizes.py
"""
import json
import sys
from collections import defaultdict

panes = json.load(sys.stdin)

tabs = defaultdict(list)
for p in panes:
    tabs[(p["window_id"], p["tab_id"], p["tab_title"])].append(p)

print(f"Total: {len(panes)} panes across {len(tabs)} tabs\n")

issues = 0
for (wid, tid, title), tab_panes in sorted(tabs.items()):
    if len(tab_panes) == 1:
        p = tab_panes[0]
        s = p["size"]
        print(f"  window={wid} tab={tid} ({title}): 1 pane, "
              f"{s['cols']}x{s['rows']} ({s['pixel_width']}x{s['pixel_height']}px)")
        continue

    pane_rects = []
    for p in tab_panes:
        s = p["size"]
        pane_rects.append({
            "pane_id": p["pane_id"],
            "title": p["title"][:20],
            "left_col": p["left_col"],
            "top_row": p["top_row"],
            "cols": s["cols"],
            "rows": s["rows"],
            "right_col": p["left_col"] + s["cols"],
            "bottom_row": p["top_row"] + s["rows"],
            "pw": s["pixel_width"],
            "ph": s["pixel_height"],
        })

    min_col = min(r["left_col"] for r in pane_rects)
    min_row = min(r["top_row"] for r in pane_rects)
    max_col = max(r["right_col"] for r in pane_rects)
    max_row = max(r["bottom_row"] for r in pane_rects)

    print(f"  window={wid} tab={tid} ({title}): {len(tab_panes)} panes")
    print(f"    bounding box: cols [{min_col}..{max_col}], rows [{min_row}..{max_row}]")

    for r in sorted(pane_rects, key=lambda r: (r["top_row"], r["left_col"])):
        print(f"    pane {r['pane_id']:>3} ({r['title']:<20}): "
              f"pos=({r['left_col']:>3},{r['top_row']:>3}) "
              f"size={r['cols']:>3}x{r['rows']:<3} "
              f"end=({r['right_col']:>3},{r['bottom_row']:>3}) "
              f"px={r['pw']}x{r['ph']}")

    # Panes sharing a left_col should have the same width
    col_groups = defaultdict(list)
    for r in pane_rects:
        col_groups[r["left_col"]].append(r)
    for col, group in col_groups.items():
        if len(group) > 1:
            widths = set(r["cols"] for r in group)
            if len(widths) > 1:
                print(f"    ** MISMATCH: panes at left_col={col} have different widths: {widths}")
                issues += 1

    # Panes sharing a top_row should have the same height
    row_groups = defaultdict(list)
    for r in pane_rects:
        row_groups[r["top_row"]].append(r)
    for row, group in row_groups.items():
        if len(group) > 1:
            heights = set(r["rows"] for r in group)
            if len(heights) > 1:
                print(f"    ** MISMATCH: panes at top_row={row} have different heights: {heights}")
                issues += 1

    print()

if issues:
    print(f"{issues} mismatch(es) found")
else:
    print("No mismatches found")
