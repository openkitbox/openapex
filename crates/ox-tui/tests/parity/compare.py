#!/usr/bin/env python3
"""Cell-level diff of two capture.py grids.

usage: compare.py GOLD.json ACTUAL.json [--max N] [--no-color]

Reports every differing cell as (row,col) with both sides, then a summary of
differing cells / total cells split into char / fg / bg / flags mismatches.
Exit status is 1 when any cell differs.
"""
import json
import sys


def load(path):
    with open(path) as f:
        return json.load(f)


def main():
    args = sys.argv[1:]
    limit = 40
    check_color = True
    if "--max" in args:
        i = args.index("--max")
        limit = int(args[i + 1])
        del args[i : i + 2]
    if "--no-color" in args:
        args.remove("--no-color")
        check_color = False
    gold, actual = load(args[0]), load(args[1])
    if (gold["rows"], gold["cols"]) != (actual["rows"], actual["cols"]):
        raise SystemExit("grid sizes differ")

    counts = {"char": 0, "fg": 0, "bg": 0, "flags": 0}
    differing = 0
    shown = 0
    rows_hit = set()
    for y in range(gold["rows"]):
        for x in range(gold["cols"]):
            g, a = gold["cells"][y][x], actual["cells"][y][x]
            bad = []
            if g[0] != a[0]:
                bad.append("char")
            blank = g[0] == " " and a[0] == " "  # fg of a blank cell is invisible
            if check_color and not blank and g[1] != a[1]:
                bad.append("fg")
            if check_color and g[2] != a[2]:
                bad.append("bg")
            if g[3] != a[3] and not (blank and set(g[3]) <= set("b") and set(a[3]) <= set("b")):
                bad.append("flags")
            if not bad:
                continue
            differing += 1
            rows_hit.add(y)
            for b in bad:
                counts[b] += 1
            if shown < limit:
                shown += 1
                print(f"({y + 1:2},{x + 1:3}) {'+'.join(bad):<12} gold={g!r} actual={a!r}")
    total = gold["rows"] * gold["cols"]
    print(
        f"\n{differing}/{total} cells differ on {len(rows_hit)} rows "
        f"(char {counts['char']}, fg {counts['fg']}, bg {counts['bg']}, flags {counts['flags']})"
    )
    sys.exit(1 if differing else 0)


if __name__ == "__main__":
    main()
