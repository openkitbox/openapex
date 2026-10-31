import json
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import extract_tui  # noqa: E402

UPSTREAM = "/Users/john/github/opencode"
UPSTREAM_SRC = UPSTREAM + "/packages/tui/src"


def run(files, kinds=None):
    """Run extract over an in-memory {path: text} tree."""

    def read_file(path):
        return files[path]

    return extract_tui.extract(read_file, lambda: list(files), kinds)


def ids(result, kind):
    return [e["id"] for e in result[kind]]


def by_id(result, kind, id_):
    return [e for e in result[kind] if e["id"] == id_][0]


KEYBIND_TS = '''import x from "y"

export const LeaderDefault = "ctrl+x"

const keybind = (value: string, description: string) => ({ default: value, description })

export const Definitions = {
  leader: keybind(LeaderDefault, "Leader key"),

  "app.exit": keybind("ctrl+c,ctrl+d,<leader>q", "Exit"),
  "which-key.group.next": keybind("ctrl+alt+right", "Next"),
} satisfies Record<string, Definition>

const other = {
  "not.a.keybind": keybind("x", "outside Definitions"),
}
'''


class KeybindTest(unittest.TestCase):
    def test_definitions_entries(self):
        r = run({"config/keybind.ts": KEYBIND_TS}, ["keybind"])
        self.assertEqual(ids(r, "keybind"), ["app.exit", "leader", "which-key.group.next"])
        self.assertEqual(by_id(r, "keybind", "leader")["line"], 8)
        self.assertEqual(by_id(r, "keybind", "app.exit")["line"], 10)
        self.assertEqual(by_id(r, "keybind", "app.exit")["file"], "config/keybind.ts")

    def test_ignores_entries_after_definitions_and_other_files(self):
        r = run({"config/keybind.ts": KEYBIND_TS, "config/v1/keybind.ts": KEYBIND_TS}, ["keybind"])
        self.assertNotIn("not.a.keybind", ids(r, "keybind"))
        self.assertEqual({e["file"] for e in r["keybind"]}, {"config/keybind.ts"})


SLASH_TSX = '''const a = [
  {
    title: "Sessions",
    slash: { name: "sessions", aliases: ["resume", "continue"] },
  },
  {
    slash: {
      name: "compact",
      aliases: [
        "summarize",
      ],
    },
  },
  { slash: { name: "rename", arguments: true as const } },
  { slash: { name: 'single' } },
  { slash: entry.command.slash },
  { slash?: { name: string } },
]
'''


class SlashTest(unittest.TestCase):
    def setUp(self):
        self.r = run({"app.tsx": SLASH_TSX}, ["slash", "slash_alias"])

    def test_slash_names(self):
        self.assertEqual(ids(self.r, "slash"), ["compact", "rename", "sessions", "single"])
        self.assertEqual(by_id(self.r, "slash", "sessions")["line"], 4)
        self.assertEqual(by_id(self.r, "slash", "compact")["line"], 8)
        self.assertEqual(by_id(self.r, "slash", "rename")["line"], 14)

    def test_aliases_including_multiline(self):
        self.assertEqual(ids(self.r, "slash_alias"), ["continue", "resume", "summarize"])
        self.assertEqual(by_id(self.r, "slash_alias", "resume")["line"], 4)
        self.assertEqual(by_id(self.r, "slash_alias", "summarize")["line"], 10)

    def test_mini_kind_slash_not_matched(self):
        r = run({"mini/x.ts": '{ kind: "slash", name: "new" }'}, ["slash"])
        self.assertEqual(r["slash"], [])


class DialogTest(unittest.TestCase):
    def test_dialog_files_anywhere(self):
        r = run(
            {
                "component/dialog-model.tsx": "x",
                "ui/dialog-alert.tsx": "x",
                "component/dialog-model.test.tsx": "x",
                "component/other.tsx": "x",
                "util/dialog-helper.ts": "x",
            },
            ["dialog"],
        )
        self.assertEqual(ids(r, "dialog"), ["dialog-alert", "dialog-model"])
        e = by_id(r, "dialog", "dialog-model")
        self.assertEqual((e["file"], e["line"]), ("component/dialog-model.tsx", 1))


SDK_TSX = '''const x = await client.api.session.update({ id })
await api.session.interrupt(id)
  client.api.worktree.list(
client.connection.status()
client.event.on("a.b", h)
data.session.get(id)
const host = { client: { api } }
await props.context.client.session.command({})
'''


class SdkCallTest(unittest.TestCase):
    def test_api_prefixed_calls_only(self):
        r = run({"app.tsx": SDK_TSX}, ["sdk_call"])
        self.assertEqual(ids(r, "sdk_call"), ["session.interrupt", "session.update", "worktree.list"])
        self.assertEqual(by_id(r, "sdk_call", "session.update")["line"], 1)
        self.assertEqual(by_id(r, "sdk_call", "worktree.list")["line"], 3)


EVENT_TSX = '''client.event.on("tui.prompt.append", (e) => {})
data.on("session.text.delta", (event) => {})
if (event.type === "worktree.updated") {}
switch (event.type) {
  case "plugin.updated":
}
area.on("line-info-change", f)
emitter.on("data", f)
'''


class SseEventTest(unittest.TestCase):
    def test_literals(self):
        r = run({"context/event.ts": EVENT_TSX}, ["sse_event"])
        self.assertEqual(
            ids(r, "sse_event"),
            ["plugin.updated", "session.text.delta", "tui.prompt.append", "worktree.updated"],
        )
        self.assertEqual(by_id(r, "sse_event", "tui.prompt.append")["line"], 1)
        self.assertEqual(by_id(r, "sse_event", "plugin.updated")["line"], 5)


THEME_V1 = json.dumps({"$schema": "x", "defs": {"darkStep9": "#fff"}, "theme": {"primary": {"dark": "a", "light": "b"}, "accent": {}}}, indent=2)
THEME_V2 = json.dumps(
    {
        "base": {
            "categorical": ["blue"],
            "text": {"base": "$x", "action": {"primary": {"base": "$y", "$hovered": "$z"}}},
        },
        "light": {},
        "dark": {},
    },
    indent=2,
)


class ThemeTokenTest(unittest.TestCase):
    def test_v1_flat_keys_with_lines(self):
        r = run({"theme/assets/opencode.json": THEME_V1}, ["theme_token"])
        self.assertEqual(ids(r, "theme_token"), ["accent", "primary"])
        self.assertEqual(by_id(r, "theme_token", "primary")["line"], 7)
        self.assertEqual(by_id(r, "theme_token", "primary")["file"], "theme/assets/opencode.json")

    def test_v2_leaf_paths(self):
        r = run({"theme/assets/v2/opencode.json": THEME_V2}, ["theme_token_v2"])
        self.assertEqual(
            ids(r, "theme_token_v2"),
            ["categorical", "text.action.primary.$hovered", "text.action.primary.base", "text.base"],
        )

    def test_bad_json_is_skipped(self):
        r = run({"theme/assets/opencode.json": "{not json"}, ["theme_token"])
        self.assertEqual(r["theme_token"], [])


TOOLS_TSX = '''import x from "y"

const toolDisplays = new Set([
  "shell",
  "glob",
  "skill",
])

export function toolDisplay() {}
'''


class ToolRendererTest(unittest.TestCase):
    def test_tool_displays_set(self):
        r = run({"routes/session/message-parts.tsx": TOOLS_TSX}, ["tool_renderer"])
        self.assertEqual(ids(r, "tool_renderer"), ["glob", "shell", "skill"])
        self.assertEqual(by_id(r, "tool_renderer", "shell")["line"], 4)


class CoreTest(unittest.TestCase):
    def test_dedup_keeps_first_and_sorts(self):
        files = {
            "b.tsx": 'x = { slash: { name: "dup" } }',
            "a.tsx": '\n\nx = { slash: { name: "dup" } }',
        }
        r = run(files, ["slash"])
        self.assertEqual(len(r["slash"]), 1)
        self.assertEqual(r["slash"][0]["file"], "a.tsx")

    def test_test_files_excluded(self):
        files = {
            "x.test.tsx": 'x = { slash: { name: "t1" } }',
            "__tests__/y.tsx": 'x = { slash: { name: "t2" } }',
            "z.tsx": 'x = { slash: { name: "ok" } }',
        }
        self.assertEqual(ids(run(files, ["slash"]), "slash"), ["ok"])

    def test_mini_not_excluded(self):
        r = run({"mini/a.tsx": 'x = { slash: { name: "m" } }'}, ["slash"])
        self.assertEqual(ids(r, "slash"), ["m"])

    def test_read_failure_skipped(self):
        def read_file(path):
            if path == "bad.tsx":
                raise OSError("boom")
            return 'x = { slash: { name: "ok" } }'

        r = extract_tui.extract(read_file, lambda: ["bad.tsx", "good.tsx"], ["slash"])
        self.assertEqual(ids(r, "slash"), ["ok"])

    def test_default_returns_all_kinds(self):
        r = run({})
        self.assertEqual(set(r), set(extract_tui.KINDS))

    def test_unknown_kind_rejected(self):
        with self.assertRaises(ValueError):
            run({}, ["nope"])


@unittest.skipUnless(os.path.isdir(UPSTREAM_SRC), "upstream opencode checkout not present")
class RealUpstreamTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        read_file, list_files = extract_tui.worktree_source(UPSTREAM)
        cls.r = extract_tui.extract(read_file, list_files)

    def test_floors(self):
        floors = FLOORS
        for kind, floor in floors.items():
            self.assertGreaterEqual(len(self.r[kind]), floor, kind)

    def test_known_ids(self):
        self.assertIn("app.exit", ids(self.r, "keybind"))
        self.assertIn("sessions", ids(self.r, "slash"))
        self.assertIn("resume", ids(self.r, "slash_alias"))
        self.assertIn("dialog-model", ids(self.r, "dialog"))
        self.assertIn("session.update", ids(self.r, "sdk_call"))
        self.assertIn("session.text.delta", ids(self.r, "sse_event"))
        self.assertIn("primary", ids(self.r, "theme_token"))
        self.assertIn("shell", ids(self.r, "tool_renderer"))

    def test_git_ref_matches_worktree_for_slash(self):
        import subprocess

        try:
            subprocess.run(["git", "-C", UPSTREAM, "rev-parse", "v2.2.3"], check=True, capture_output=True)
        except (subprocess.CalledProcessError, OSError):
            self.skipTest("tag v2.2.3 missing")
        read_file, list_files = extract_tui.git_source(UPSTREAM, "v2.2.3")
        r = extract_tui.extract(read_file, list_files, ["slash"])
        self.assertIn("sessions", ids(r, "slash"))


FLOORS = {
    # ~80% of counts measured on upstream HEAD 173f6c6dce (2026-10-31)
    "keybind": 195,
    "slash": 30,
    "slash_alias": 7,
    "dialog": 29,
    "sdk_call": 19,
    "sse_event": 52,
    "theme_token": 40,
    "theme_token_v2": 60,
    "tool_renderer": 11,
}

if __name__ == "__main__":
    unittest.main()
