#!/usr/bin/env python3
"""Extract "feature IDs" from the upstream opencode TUI source (packages/tui/src).

The IDs feed the parity ledger (features.py): every id found here must be
registered there as ported / n/a / gap, so a new upstream feature cannot slip
through unnoticed.

Usage:
    extract_tui.py [--root /Users/john/github/opencode] [--ref TAG] [--kind K ...]

Prints JSON ``{kind: [{"id", "file", "line"}]}``. ``file`` is relative to
packages/tui/src. An empty ``--ref`` (default) reads the working tree; a
non-empty one reads that git ref through ``git ls-tree`` / ``git show``.
Ids are deduplicated within a kind (first occurrence wins, files visited in
path order) and sorted. A file that cannot be read is skipped.

General rules
-------------
* Test files (``*.test.ts(x)``, ``__tests__/``) are excluded.
* ``mini/`` is NOT excluded. It is the upstream mini-TUI; whether a feature
  there matters to ox is a ledger decision (mark it n/a there). Ids keep the
  ``file`` path, so the ledger can tell mini apart. In practice mini uses
  other syntaxes (``{ kind: "slash", ... }``, ``sdk.X.Y(``), which the
  patterns below deliberately do not match, so mini adds little noise.
* Everything is regex over source text, not a TS parse. Each limit below is
  a place where a refactor upstream could make a feature invisible; the
  RealUpstreamTest count floors in tests/ are the tripwire for that.

Kinds
-----
keybind
    Entries of ``export const Definitions = { ... }`` in config/keybind.ts:
    ``"app.exit": keybind(...)`` and the unquoted ``leader: keybind(...)``.
    ``config/v1/keybind.ts`` (underscore names such as ``app_exit``) is
    IGNORED: runtime TUI code does not read it, only the CLI config
    migration (packages/cli/src/config/migrate.ts) and tests do.

slash, slash_alias
    ``slash: { name: "x", aliases: ["a", "b"] }`` inside command
    registrations, single- or multi-line, single/double/back-quoted literal.
    ``slash_alias`` ids are the alias strings. Not matched: ``slash: expr``
    (keymap.tsx), ``slash?: {`` types, and mini's ``{ kind: "slash" }``.
    Limit: names built from template expressions are skipped.

dialog
    Files named ``dialog-*.tsx`` anywhere in the tree. id = basename without
    extension, line = 1. Limit: a dialog defined inside a non-dialog file is
    invisible.

sdk_call
    HTTP SDK calls of the form ``client.api.<ns>.<method>(`` or
    ``api.<ns>.<method>(``; id = ``<ns>.<method>`` (e.g. ``session.update``).
    Spot-checked against packages/protocol/openapi.json (2026-10-31):
    session.update (PATCH /api/session/{sessionID}), session.interrupt,
    session.background, session.view, session.move, session.switchModel,
    session.synthetic, worktree.list, worktree.refresh, shell.output,
    server.info, server.pair, location.get, credential.activate are all
    real operations. Naming differs for file.*: the SDK says ``file.list`` /
    ``file.find`` while openapi operationIds are ``fs.list`` / ``fs.find``;
    ids here follow the SDK surface the TUI source uses.
    NOT server calls, hence not extracted: ``client.connection.*``,
    ``client.event.on/listen`` (local subscription), ``client.persistentPty.*``,
    ``client.media.read``, ``data.session.*`` (local store).
    Known blind spots: plugin code calls ``props.context.client.<ns>.<m>(``
    without ``.api`` (feature-plugins/system/*, prompt/goal.tsx), and mini
    uses ``sdk.<ns>.<m>(``. Both are skipped, so those calls must be
    registered by hand in the ledger. Only two-level ``ns.method`` is read.

sse_event
    Event type string literals used at subscription/handling sites:
    ``.on("x.y", ...)`` / ``.listen("x.y", ...)`` (client.event.on and the
    session data store ``data.on``, e.g. ``session.text.delta`` in
    routes/session/rows.ts), ``.type === "x.y"`` / ``!==``, and
    ``case "x.y":`` in files that test ``.type``. Only dotted lowercase names
    match, which filters out DOM-style events (``line-info-change``).
    Because mini/ is kept, mini-internal emitter names (``turn.send``,
    ``stream.view``, ``queued.prompts`` in mini/footer.ts) show up too; they
    are not SSE events, so mark them n/a in the ledger.
    The full event vocabulary lives in ``@opencode/client`` outside this tree;
    types handled through generic code or computed names are not seen.

theme_token
    Flat keys of the ``theme`` object in theme/assets/opencode.json (the v1
    token set, also produced by theme/system.ts). The authority for these
    names is packages/theme/src/tui/v1.ts, outside this tree, so this is a
    proxy. line = line of the key in the JSON file.

theme_token_v2
    Leaf paths under ``base`` in theme/assets/v2/opencode.json, e.g.
    ``text.action.primary.base`` or ``text.action.primary.$hovered``. Lists
    (``categorical``) count as leaves. Authority: packages/theme/src schema.

tool_renderer
    String members of ``const toolDisplays = new Set([...])`` in
    routes/session/message-parts.tsx: tools with a dedicated renderer; any
    other tool falls back to GenericTool. Aliases (bash->shell, task->subagent,
    apply_patch->patch) live in util/tool-display.ts and are not ids.

Not extractable (register by hand in the ledger)
------------------------------------------------
part_type
    Assistant content part types (text / reasoning / tool, tool states
    streaming / running / completed / error) come from ``@opencode/client``,
    outside this tree. In-tree they only appear as a ``<Match when=
    {item().type === "text"}>`` chain in routes/session/index.tsx, which
    shows what the TUI handles, not what exists. Not reliable, so omitted.
cli_option
    Flags are defined in packages/cli/src/commands/commands.ts with
    ``Flag.String("prompt")`` style Effect CLI builders, not in the TUI tree.
    The TUI only sees the result as the ``Args`` interface in context/args.tsx
    (model, agent, prompt, continue, sessionID, newSessionID, fork, auto).
    Extracting from the CLI package would need a second root, so omitted.
"""

import argparse
import bisect
import json
import os
import re
import subprocess
import sys

TUI_SUBDIR = "packages/tui/src"
DEFAULT_ROOT = "/Users/john/github/opencode"

KINDS = (
    "keybind",
    "slash",
    "slash_alias",
    "dialog",
    "sdk_call",
    "sse_event",
    "theme_token",
    "theme_token_v2",
    "tool_renderer",
)

# --- path helpers ------------------------------------------------------------


def is_test_file(path):
    return "__tests__" in path.split("/") or re.search(r"\.test\.tsx?$", path) is not None


def is_source(path):
    return path.endswith(".ts") or path.endswith(".tsx")


def line_of(offsets, pos):
    """1-based line number of character offset ``pos``; offsets = newline positions."""
    return bisect.bisect_left(offsets, pos) + 1


def newline_offsets(text):
    return [m.start() for m in re.finditer("\n", text)]


def entry(id_, path, line):
    return {"id": id_, "file": path, "line": line}


# --- per-kind extractors: (path, text) -> [(kind, id, line)] -----------------

SLASH_RE = re.compile(
    r"slash:\s*\{\s*name:\s*([\"'`])([^\"'`$\\]+)\1\s*,?\s*(?:aliases:\s*\[([^\]]*)\])?"
)
ALIAS_RE = re.compile(r"([\"'`])([^\"'`$\\]+)\1")
KEYBIND_ENTRY_RE = re.compile(r"^\s*(?:\"([^\"]+)\"|([A-Za-z_]\w*))\s*:\s*keybind\(")
SDK_RE = re.compile(r"(?<![\w$])api\.(\w+)\.(\w+)\(")
EVENT_RES = (
    re.compile(r"\.(?:on|listen)\(\s*\"([a-z][\w-]*(?:\.[\w-]+)+)\""),
    re.compile(r"\.type\s*[!=]==?\s*\"([a-z][\w-]*(?:\.[\w-]+)+)\""),
)
CASE_RE = re.compile(r"\bcase\s+\"([a-z][\w-]*(?:\.[\w-]+)+)\"\s*:")
STRING_RE = re.compile(r"\"([^\"]+)\"")


def keybinds(path, text):
    if path != "config/keybind.ts":
        return
    inside = False
    for n, line in enumerate(text.split("\n"), 1):
        if not inside:
            inside = line.startswith("export const Definitions = {")
            continue
        if line.startswith("}"):
            return
        m = KEYBIND_ENTRY_RE.match(line)
        if m:
            yield "keybind", m.group(1) or m.group(2), n


def slashes(path, text):
    offsets = newline_offsets(text)
    for m in SLASH_RE.finditer(text):
        yield "slash", m.group(2), line_of(offsets, m.start(2))
        if m.group(3):
            for a in ALIAS_RE.finditer(m.group(3)):
                yield "slash_alias", a.group(2), line_of(offsets, m.start(3) + a.start(2))


def dialogs(path, text):
    base = os.path.basename(path)
    if base.startswith("dialog-") and base.endswith(".tsx"):
        yield "dialog", base[: -len(".tsx")], 1


def sdk_calls(path, text):
    offsets = newline_offsets(text)
    for m in SDK_RE.finditer(text):
        yield "sdk_call", m.group(1) + "." + m.group(2), line_of(offsets, m.start())


def sse_events(path, text):
    offsets = newline_offsets(text)
    regexes = list(EVENT_RES)
    if ".type" in text:
        regexes.append(CASE_RE)
    for rx in regexes:
        for m in rx.finditer(text):
            yield "sse_event", m.group(1), line_of(offsets, m.start(1))


def tool_renderers(path, text):
    if path != "routes/session/message-parts.tsx":
        return
    start = text.find("const toolDisplays = new Set([")
    if start < 0:
        return
    end = text.find("])", start)
    offsets = newline_offsets(text)
    for m in STRING_RE.finditer(text, start, end):
        yield "tool_renderer", m.group(1), line_of(offsets, m.start(1))


def key_line(lines, segments):
    """Line of the last segment, searching each segment after the previous one."""
    n = 0
    for seg in segments:
        needle = json.dumps(seg) + ":"
        while n < len(lines) and needle not in lines[n]:
            n += 1
        if n >= len(lines):
            return 1
    return n + 1


def leaf_paths(node, prefix=()):
    if isinstance(node, dict):
        for k, v in node.items():
            for p in leaf_paths(v, prefix + (k,)):
                yield p
    else:
        yield prefix


def theme_tokens(path, text):
    if path == "theme/assets/opencode.json":
        kind, root_key, flat = "theme_token", "theme", True
    elif path == "theme/assets/v2/opencode.json":
        kind, root_key, flat = "theme_token_v2", "base", False
    else:
        return
    try:
        root = json.loads(text)[root_key]
    except (ValueError, KeyError, TypeError):
        return
    if not isinstance(root, dict):
        return
    lines = text.split("\n")
    paths = [(k,) for k in root] if flat else list(leaf_paths(root))
    for p in paths:
        yield kind, ".".join(p), key_line(lines, (root_key,) + p)


TEXT_EXTRACTORS = (
    (("keybind",), keybinds),
    (("slash", "slash_alias"), slashes),
    (("dialog",), dialogs),
    (("sdk_call",), sdk_calls),
    (("sse_event",), sse_events),
    (("tool_renderer",), tool_renderers),
)
JSON_KINDS = ("theme_token", "theme_token_v2")

# --- core --------------------------------------------------------------------


def extract(read_file, list_files, kinds=None):
    """Pure core. read_file(path) -> str; list_files() -> [path] (relative to tui/src)."""
    wanted = tuple(kinds) if kinds else KINDS
    unknown = [k for k in wanted if k not in KINDS]
    if unknown:
        raise ValueError("unknown kind(s): " + ", ".join(unknown))
    out = {k: [] for k in wanted}
    seen = {k: set() for k in wanted}
    for path in sorted(list_files()):
        if is_test_file(path):
            continue
        extractors = [fn for ks, fn in TEXT_EXTRACTORS if is_source(path) and any(k in wanted for k in ks)]
        if path.endswith(".json") and any(k in wanted for k in JSON_KINDS):
            extractors.append(theme_tokens)
        if not extractors:
            continue
        try:
            text = read_file(path)
        except (OSError, subprocess.CalledProcessError, UnicodeDecodeError):
            continue
        for fn in extractors:
            for kind, id_, line in fn(path, text):
                if kind in out and id_ not in seen[kind]:
                    seen[kind].add(id_)
                    out[kind].append(entry(id_, path, line))
    for items in out.values():
        items.sort(key=lambda e: e["id"])
    return out


# --- sources -----------------------------------------------------------------


def worktree_source(root):
    base = os.path.join(root, TUI_SUBDIR)

    def list_files():
        found = []
        for dirpath, _dirs, names in os.walk(base):
            for name in names:
                found.append(os.path.relpath(os.path.join(dirpath, name), base))
        return found

    def read_file(path):
        with open(os.path.join(base, path), encoding="utf-8") as f:
            return f.read()

    return read_file, list_files


def git_source(root, ref):
    def list_files():
        res = subprocess.run(
            ["git", "-C", root, "ls-tree", "-r", "--name-only", ref, "--", TUI_SUBDIR],
            check=True,
            capture_output=True,
            text=True,
        )
        prefix = TUI_SUBDIR + "/"
        return [p[len(prefix):] for p in res.stdout.splitlines() if p.startswith(prefix)]

    def read_file(path):
        res = subprocess.run(
            ["git", "-C", root, "show", "%s:%s/%s" % (ref, TUI_SUBDIR, path)],
            check=True,
            capture_output=True,
        )
        return res.stdout.decode("utf-8")

    return read_file, list_files


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--root", default=DEFAULT_ROOT)
    ap.add_argument("--ref", default="", help="git ref; empty reads the working tree")
    ap.add_argument("--kind", action="append", choices=KINDS, help="repeatable; default all")
    args = ap.parse_args(argv)
    read_file, list_files = git_source(args.root, args.ref) if args.ref else worktree_source(args.root)
    json.dump(extract(read_file, list_files, args.kind), sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
