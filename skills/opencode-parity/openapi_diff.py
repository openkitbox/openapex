#!/usr/bin/env python3
"""Protocol-layer diff between two upstream opencode openapi.json documents.

Usage:
    openapi_diff.py OLD_TAG NEW_TAG [--root /Users/john/github/opencode] [--json]
    openapi_diff.py --old-file PATH --new-file PATH [--json]

``diff(old, new)`` is a pure function over two parsed documents. Result keys
(lists sorted, keys stable):

    paths_added / paths_removed            path strings
    operations_added / operations_removed  "METHOD /path"
    operations_changed                     [{"op": "METHOD /path", "changes": [...]}]
    schemas_added / schemas_removed        components.schemas keys
    schemas_changed                        [{"schema": name, "changes": [...]}]

Comparison rules: ``$ref`` is compared as a string and never expanded; inline
schemas are compared structurally and reported only as "inline schema changed".
Parameters are keyed by (in, name); path-level parameters are merged with
operation-level ones, the operation winning. Schema changes cover the
``properties`` key set, ``required``, and ``type``/``enum``/``oneOf``/``anyOf``/
``allOf``; a changed property definition is not reported.
"""

import argparse
import json
import subprocess
import sys

DEFAULT_ROOT = "/Users/john/github/opencode"
OPENAPI_PATH = "packages/protocol/openapi.json"
METHODS = ("get", "put", "post", "delete", "patch", "head", "options")
SCHEMA_KEYS = ("type", "enum", "oneOf", "anyOf", "allOf")


def schema_change(old, new):
    """Describe a difference between two schema objects, or None when equal."""
    if old == new:
        return None
    old_ref = old.get("$ref") if isinstance(old, dict) else None
    new_ref = new.get("$ref") if isinstance(new, dict) else None
    if old_ref or new_ref:
        return "schema changed: %s -> %s" % (old_ref or "inline", new_ref or "inline")
    paths = []
    _walk_diff(old, new, "", paths)
    if not paths:
        return "inline schema changed"
    shown = "; ".join(paths[:4])
    more = " (+%d more)" % (len(paths) - 4) if len(paths) > 4 else ""
    return "inline schema changed: %s%s" % (shown, more)


def _walk_diff(old, new, path, out):
    """Collect the JSON paths where two inline schemas differ (leaf level)."""
    if old == new:
        return
    if isinstance(old, dict) and isinstance(new, dict):
        for key in sorted(set(old) | set(new)):
            sub = "%s/%s" % (path, key)
            if key not in old:
                out.append("+%s" % sub)
            elif key not in new:
                out.append("-%s" % sub)
            else:
                _walk_diff(old[key], new[key], sub, out)
    elif isinstance(old, list) and isinstance(new, list) and len(old) == len(new):
        for index, (a, b) in enumerate(zip(old, new)):
            _walk_diff(a, b, "%s[%d]" % (path, index), out)
    else:
        out.append("~%s" % (path or "/"))


def media_changes(old_content, new_content, prefix):
    """Changes between two ``content`` maps; ``prefix`` names the owner."""
    out = []
    for mt in sorted(set(new_content) - set(old_content)):
        out.append("%s media type added: %s" % (prefix, mt))
    for mt in sorted(set(old_content) - set(new_content)):
        out.append("%s media type removed: %s" % (prefix, mt))
    for mt in sorted(set(old_content) & set(new_content)):
        c = schema_change(old_content[mt].get("schema", {}), new_content[mt].get("schema", {}))
        if c:
            out.append("%s %s: %s" % (prefix, mt, c))
    return out


def param_key(p):
    if "$ref" in p:
        return ("$ref", p["$ref"])
    return (p.get("in", ""), p.get("name", ""))


def merged_params(path_item, op):
    merged = {}
    for p in path_item.get("parameters", []) + op.get("parameters", []):
        merged[param_key(p)] = p
    return merged


def param_changes(old_params, new_params):
    out = []
    for k in sorted(set(new_params) - set(old_params)):
        out.append("parameter added: %s %s" % k)
    for k in sorted(set(old_params) - set(new_params)):
        out.append("parameter removed: %s %s" % k)
    for k in sorted(set(old_params) & set(new_params)):
        a, b = old_params[k], new_params[k]
        if bool(a.get("required")) != bool(b.get("required")):
            out.append("parameter %s %s: required changed" % (k[0], k[1]))
        c = schema_change(a.get("schema", {}), b.get("schema", {}))
        if c:
            out.append("parameter %s %s: %s" % (k[0], k[1], c))
    return out


def body_changes(old, new):
    if old is None and new is None:
        return []
    if old is None:
        return ["requestBody added"]
    if new is None:
        return ["requestBody removed"]
    out = []
    if bool(old.get("required")) != bool(new.get("required")):
        out.append("requestBody required changed")
    out += media_changes(old.get("content", {}), new.get("content", {}), "requestBody")
    return out


def response_changes(old, new):
    out = []
    for code in sorted(set(new) - set(old)):
        out.append("response added: %s" % code)
    for code in sorted(set(old) - set(new)):
        out.append("response removed: %s" % code)
    for code in sorted(set(old) & set(new)):
        a, b = old[code], new[code]
        if "$ref" in a or "$ref" in b:
            if a.get("$ref") != b.get("$ref"):
                out.append("response %s: ref changed" % code)
            continue
        out += media_changes(a.get("content", {}), b.get("content", {}), "response %s" % code)
    return out


def operation_changes(old_item, old_op, new_item, new_op):
    out = []
    for field in ("operationId", "summary"):
        a, b = old_op.get(field), new_op.get(field)
        if a != b:
            if field == "operationId":
                out.append("operationId changed: %s -> %s" % (a, b))
            else:
                out.append("summary changed")
    if bool(old_op.get("deprecated")) != bool(new_op.get("deprecated")):
        out.append("deprecated changed")
    out += param_changes(merged_params(old_item, old_op), merged_params(new_item, new_op))
    out += body_changes(old_op.get("requestBody"), new_op.get("requestBody"))
    out += response_changes(old_op.get("responses", {}), new_op.get("responses", {}))
    return out


def operations(paths):
    """Map "METHOD /path" -> (path_item, operation)."""
    ops = {}
    for path, item in paths.items():
        for method in METHODS:
            if isinstance(item.get(method), dict):
                ops["%s %s" % (method.upper(), path)] = (item, item[method])
    return ops


def schema_changes(old, new):
    out = []
    old_props, new_props = old.get("properties", {}), new.get("properties", {})
    out += ["property added: %s" % k for k in sorted(set(new_props) - set(old_props))]
    out += ["property removed: %s" % k for k in sorted(set(old_props) - set(new_props))]
    old_req, new_req = set(old.get("required", [])), set(new.get("required", []))
    out += ["required added: %s" % k for k in sorted(new_req - old_req)]
    out += ["required removed: %s" % k for k in sorted(old_req - new_req)]
    for key in SCHEMA_KEYS:
        if old.get(key) != new.get(key):
            out.append("%s changed" % key)
    return out


def diff(old, new):
    old_paths, new_paths = old.get("paths", {}), new.get("paths", {})
    old_ops, new_ops = operations(old_paths), operations(new_paths)
    changed = []
    for name in sorted(set(old_ops) & set(new_ops)):
        changes = operation_changes(*old_ops[name], *new_ops[name])
        if changes:
            changed.append({"op": name, "changes": changes})
    old_schemas = old.get("components", {}).get("schemas", {})
    new_schemas = new.get("components", {}).get("schemas", {})
    schemas_changed = []
    for name in sorted(set(old_schemas) & set(new_schemas)):
        changes = schema_changes(old_schemas[name], new_schemas[name])
        if changes:
            schemas_changed.append({"schema": name, "changes": changes})
    return {
        "paths_added": sorted(set(new_paths) - set(old_paths)),
        "paths_removed": sorted(set(old_paths) - set(new_paths)),
        "operations_added": sorted(set(new_ops) - set(old_ops)),
        "operations_removed": sorted(set(old_ops) - set(new_ops)),
        "operations_changed": changed,
        "schemas_added": sorted(set(new_schemas) - set(old_schemas)),
        "schemas_removed": sorted(set(old_schemas) - set(new_schemas)),
        "schemas_changed": schemas_changed,
    }


def format_text(result):
    lines = []
    for key, items in result.items():
        if not items:
            continue
        lines.append("%s (%d)" % (key, len(items)))
        for item in items:
            if isinstance(item, dict):
                name = item.get("op") or item.get("schema")
                lines.append("  %s" % name)
                lines += ["    - %s" % c for c in item["changes"]]
            else:
                lines.append("  %s" % item)
    return "\n".join(lines) if lines else "无协议层变化"


def load_file(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError) as e:
        raise SystemExit("error: cannot read %s: %s" % (path, e))


def load_tag(root, tag):
    spec = "%s:%s" % (tag, OPENAPI_PATH)
    try:
        p = subprocess.run(
            ["git", "-C", root, "show", spec],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
        )
    except OSError as e:
        raise SystemExit("error: cannot run git: %s" % e)
    if p.returncode != 0:
        raise SystemExit("error: git show %s failed in %s: %s" % (spec, root, p.stderr.strip()))
    try:
        return json.loads(p.stdout)
    except ValueError as e:
        raise SystemExit("error: %s is not valid JSON: %s" % (spec, e))


def main(argv=None):
    ap = argparse.ArgumentParser(description="Diff upstream opencode openapi.json between two tags.")
    ap.add_argument("old_tag", nargs="?")
    ap.add_argument("new_tag", nargs="?")
    ap.add_argument("--root", default=DEFAULT_ROOT)
    ap.add_argument("--old-file")
    ap.add_argument("--new-file")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args(argv)
    if args.old_file and args.new_file and not (args.old_tag or args.new_tag):
        old, new = load_file(args.old_file), load_file(args.new_file)
    elif args.old_tag and args.new_tag and not (args.old_file or args.new_file):
        old, new = load_tag(args.root, args.old_tag), load_tag(args.root, args.new_tag)
    else:
        ap.error("give OLD_TAG NEW_TAG, or both --old-file and --new-file")
    result = diff(old, new)
    print(json.dumps(result, indent=2, ensure_ascii=False) if args.json else format_text(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
