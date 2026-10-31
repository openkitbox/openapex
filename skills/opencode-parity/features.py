#!/usr/bin/env python3
"""Parity ledger (features.md): seed, check and delta over extract_tui.py output.

The ledger is the skill's only persistent state; nothing else is cached.

Usage:
    features.py [--ledger PATH] [--root /Users/john/github/opencode] seed [--ref TAG]
    features.py [--ledger PATH] [--root ...] check [--ref TAG]
    features.py [--ledger PATH] [--root ...] delta OLD_TAG NEW_TAG

Ledger format
-------------
Markdown. A line ``基线: <tag>`` (upstream tag the ledger was last aligned to),
a fixed paragraph describing the statuses, then one table::

    | kind | id | 上游 | ox | 状态 | 备注 |

``上游`` / ``ox`` hold ``file:line`` or nothing. 状态 is one of aligned,
partial, missing, deviation, n/a, todo; ``deviation`` and ``n/a`` need a
non-empty 备注 (the reason). In a cell, ``|`` is written ``\\|`` and ``\\``
is written ``\\\\``. Rows are rendered sorted by (kind, id) so diffs stay
stable; ``render_ledger(parse_ledger(t)) == t`` for rendered text. Lines
outside the baseline line and the table are ignored on parse (and dropped
on the next write).

Kinds the extractor cannot produce (part_type, cli_option, ...) are
registered by hand with the same row shape. ``check`` never reports a row as
removed unless its kind is present in the extraction result.

Semantics
---------
seed   appends ``todo`` rows for extracted (kind, id) the ledger lacks; rows
       that exist are never modified. Creates the ledger when absent; an
       existing baseline is kept.
check  missing = extracted but not in ledger (exit 1); invalid = deviation/n/a
       without a reason, or unknown status (exit 1); removed = ledger row whose
       kind was extracted but whose id is gone upstream (informational only).
delta  set difference between two extractions, each id annotated with its
       ledger row (or None).
"""

import argparse
import collections
import os
import subprocess
import sys

import extract_tui

DEFAULT_LEDGER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "features.md")
STATUSES = ("aligned", "partial", "missing", "deviation", "n/a", "todo")
NEEDS_REASON = ("deviation", "n/a")
COLUMNS = ("kind", "id", "upstream", "ox", "status", "note")
HEADER = ("kind", "id", "上游", "ox", "状态", "备注")
BASELINE_PREFIX = "基线:"

PREAMBLE = (
    "# 功能台账\n"
    "\n"
    "{baseline}\n"
    "\n"
    "状态：`aligned` 已对齐；`partial` 部分对齐；`missing` ox 没有；"
    "`deviation` 有意偏差（备注必须写原因）；`n/a` 不适用（备注必须写原因）；"
    "`todo` 刚登记，待核对。\n"
    "\n"
)

Report = collections.namedtuple("Report", "missing removed invalid")
Delta = collections.namedtuple("Delta", "added removed")


class LedgerError(Exception):
    pass


# --- ledger text <-> rows (pure) ----------------------------------------------


def escape_cell(value):
    return value.replace("\\", "\\\\").replace("|", "\\|").replace("\n", " ")


def split_cells(line):
    """Split one table line on unescaped ``|``; returns unescaped, stripped cells."""
    cells, cur, i = [], [], 0
    while i < len(line):
        ch = line[i]
        if ch == "\\" and i + 1 < len(line) and line[i + 1] in "\\|":
            cur.append(line[i + 1])
            i += 2
            continue
        if ch == "|":
            cells.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
        i += 1
    cells.append("".join(cur))
    return [c.strip(" ") for c in cells]


def parse_ledger(text):
    meta, rows, seen = {}, [], {}
    header_seen = False
    for n, raw in enumerate(text.split("\n"), 1):
        line = raw.rstrip("\r")
        if line.startswith(BASELINE_PREFIX):
            meta["baseline"] = line[len(BASELINE_PREFIX):].strip()
            continue
        if not line.startswith("|"):
            continue
        cells = split_cells(line.strip())
        if cells[0] != "" or cells[-1] != "":
            raise LedgerError("line %d: table row must start and end with '|'" % n)
        cells = cells[1:-1]
        if not header_seen:
            if tuple(cells) != HEADER:
                raise LedgerError("line %d: expected table header %s" % (n, " | ".join(HEADER)))
            header_seen = True
            continue
        if all(c and set(c) <= set("-:") for c in cells):
            continue
        if len(cells) != len(COLUMNS):
            raise LedgerError("line %d: expected %d columns, got %d" % (n, len(COLUMNS), len(cells)))
        r = dict(zip(COLUMNS, cells))
        if r["status"] not in STATUSES:
            raise LedgerError("line %d: unknown status %r" % (n, r["status"]))
        key = (r["kind"], r["id"])
        if key in seen:
            raise LedgerError("line %d: duplicate row %s/%s (first at line %d)" % (n, key[0], key[1], seen[key]))
        seen[key] = n
        rows.append(r)
    if "baseline" not in meta:
        raise LedgerError("missing '%s <tag>' line" % BASELINE_PREFIX)
    return meta, rows


def render_ledger(meta, rows):
    out = [PREAMBLE.format(baseline="%s %s" % (BASELINE_PREFIX, meta["baseline"]))]
    out.append("| " + " | ".join(HEADER) + " |\n")
    out.append("|" + "|".join("---" for _ in HEADER) + "|\n")
    for r in sorted(rows, key=lambda r: (r["kind"], r["id"])):
        out.append("| " + " | ".join(escape_cell(r[c]) for c in COLUMNS) + " |\n")
    return "".join(out)


# --- decisions (pure) ------------------------------------------------------------


def new_row(kind, id_, upstream="", status="todo"):
    return {"kind": kind, "id": id_, "upstream": upstream, "ox": "", "status": status, "note": ""}


def location(e):
    return "%s:%d" % (e["file"], e["line"])


def seed(rows, extracted):
    have = {(r["kind"], r["id"]) for r in rows}
    out = list(rows)
    for kind in sorted(extracted):
        for e in extracted[kind]:
            if (kind, e["id"]) not in have:
                out.append(new_row(kind, e["id"], location(e)))
    return out


def extracted_keys(extracted):
    return {(k, e["id"]) for k, items in extracted.items() for e in items}


def check(rows, extracted):
    keys = extracted_keys(extracted)
    have = {(r["kind"], r["id"]) for r in rows}
    missing = sorted(keys - have)
    removed = sorted(
        (r["kind"], r["id"]) for r in rows if r["kind"] in extracted and (r["kind"], r["id"]) not in keys
    )
    invalid = []
    for r in sorted(rows, key=lambda r: (r["kind"], r["id"])):
        if r["status"] not in STATUSES:
            invalid.append((r["kind"], r["id"], "unknown status %r" % r["status"]))
        elif r["status"] in NEEDS_REASON and not r["note"].strip():
            invalid.append((r["kind"], r["id"], "%s needs a reason in 备注" % r["status"]))
    return Report(missing, removed, invalid)


def exit_code(report):
    return 1 if report.missing or report.invalid else 0


def kind_counts(rows, extracted):
    """[(kind, extracted_count or None, ledger_count)] sorted by kind."""
    ledger = collections.Counter(r["kind"] for r in rows)
    kinds = sorted(set(ledger) | set(extracted))
    return [(k, len(extracted[k]) if k in extracted else None, ledger.get(k, 0)) for k in kinds]


def delta(old_extracted, new_extracted, rows):
    by_key = {(r["kind"], r["id"]): r for r in rows}

    def only_in(a, b):
        known = extracted_keys(b)
        found = []
        for kind in sorted(a):
            for e in a[kind]:
                if (kind, e["id"]) not in known:
                    found.append(
                        {"kind": kind, "id": e["id"], "file": e["file"], "line": e["line"],
                         "ledger": by_key.get((kind, e["id"]))}
                    )
        return sorted(found, key=lambda x: (x["kind"], x["id"]))

    return Delta(only_in(new_extracted, old_extracted), only_in(old_extracted, new_extracted))


# --- I/O edge ----------------------------------------------------------------------


def extract_at(root, ref):
    source = extract_tui.git_source(root, ref) if ref else extract_tui.worktree_source(root)
    return extract_tui.extract(*source)


def describe_baseline(root):
    res = subprocess.run(
        ["git", "-C", root, "describe", "--tags", "--abbrev=0"], capture_output=True, text=True
    )
    return res.stdout.strip() if res.returncode == 0 and res.stdout.strip() else "unknown"


def read_ledger(path):
    with open(path, encoding="utf-8") as f:
        return parse_ledger(f.read())


def write_ledger(path, meta, rows):
    with open(path, "w", encoding="utf-8") as f:
        f.write(render_ledger(meta, rows))


def cmd_seed(args):
    extracted = extract_at(args.root, args.ref)
    if os.path.exists(args.ledger):
        meta, rows = read_ledger(args.ledger)
    else:
        meta, rows = {"baseline": args.ref or describe_baseline(args.root)}, []
    out = seed(rows, extracted)
    write_ledger(args.ledger, meta, out)
    print("%s: baseline %s, %d rows (+%d new)" % (args.ledger, meta["baseline"], len(out), len(out) - len(rows)))
    return 0


def cmd_check(args):
    _meta, rows = read_ledger(args.ledger)
    extracted = extract_at(args.root, args.ref)
    rep = check(rows, extracted)
    print("kind: extracted / ledger")
    for kind, ext, led in kind_counts(rows, extracted):
        print("  %-16s %s / %d" % (kind, "-" if ext is None else ext, led))
    for title, items in (("MISSING (extracted, not in ledger)", rep.missing),
                         ("INVALID", rep.invalid),
                         ("removed upstream (info only)", rep.removed)):
        if items:
            print("%s: %d" % (title, len(items)))
            for it in items:
                print("  " + " ".join(str(x) for x in it))
    code = exit_code(rep)
    print("result: %s" % ("FAIL" if code else "ok"))
    return code


def cmd_delta(args):
    _meta, rows = read_ledger(args.ledger)
    d = delta(extract_at(args.root, args.old), extract_at(args.root, args.new), rows)
    for title, items in (("added in %s" % args.new, d.added), ("removed in %s" % args.new, d.removed)):
        print("%s: %d" % (title, len(items)))
        for e in items:
            r = e["ledger"]
            state = "not in ledger" if r is None else r["status"] + (" (%s)" % r["note"] if r["note"] else "")
            print("  %s %s  %s:%d  [%s]" % (e["kind"], e["id"], e["file"], e["line"], state))
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--ledger", default=DEFAULT_LEDGER)
    ap.add_argument("--root", default=extract_tui.DEFAULT_ROOT)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("seed", "check"):
        p = sub.add_parser(name)
        p.add_argument("--ref", default="", help="git ref; empty reads the working tree")
        p.set_defaults(run=cmd_seed if name == "seed" else cmd_check)
    p = sub.add_parser("delta")
    p.add_argument("old")
    p.add_argument("new")
    p.set_defaults(run=cmd_delta)
    args = ap.parse_args(argv)
    try:
        return args.run(args)
    except (LedgerError, OSError, subprocess.CalledProcessError) as e:
        print("error: %s" % e, file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
