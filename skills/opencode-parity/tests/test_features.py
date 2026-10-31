import contextlib
import io
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import features  # noqa: E402

UPSTREAM = "/Users/john/github/opencode"


def row(kind, id_, upstream="", ox="", status="todo", note=""):
    return {"kind": kind, "id": id_, "upstream": upstream, "ox": ox, "status": status, "note": note}


def ex(*ids):
    return [{"id": i, "file": "f/%s.ts" % i, "line": 3} for i in ids]


META = {"baseline": "v2.2.3"}


class RoundTripTest(unittest.TestCase):
    def test_roundtrip_stable(self):
        rows = [
            row("dialog", "dialog-a", "component/dialog-a.tsx:1", "crates/x.rs:9", "aligned", "ok"),
            row("keybind", "app.exit", "config/keybind.ts:12"),
            row("part_type", "reasoning", "", "", "n/a", "不在 TUI 树内"),
        ]
        text = features.render_ledger(META, rows)
        meta2, rows2 = features.parse_ledger(text)
        self.assertEqual(meta2, META)
        self.assertEqual(rows2, rows)
        self.assertEqual(features.render_ledger(meta2, rows2), text)

    def test_render_sorts_by_kind_then_id(self):
        rows = [row("keybind", "b"), row("dialog", "z"), row("keybind", "a")]
        _, parsed = features.parse_ledger(features.render_ledger(META, rows))
        self.assertEqual([(r["kind"], r["id"]) for r in parsed], [("dialog", "z"), ("keybind", "a"), ("keybind", "b")])

    def test_pipe_and_backslash_escaped(self):
        rows = [row("x", "y", status="deviation", note="a | b \\ c \\| d \\")]
        text = features.render_ledger(META, rows)
        self.assertIn("a \\| b", text)
        _, parsed = features.parse_ledger(text)
        self.assertEqual(parsed, rows)

    def test_empty_ledger(self):
        text = features.render_ledger(META, [])
        self.assertEqual(features.parse_ledger(text), (META, []))


class MalformedTest(unittest.TestCase):
    def good(self):
        return features.render_ledger(META, [row("keybind", "a")]).split("\n")

    def parse_lines(self, lines):
        return features.parse_ledger("\n".join(lines))

    def test_wrong_column_count_reports_line(self):
        lines = self.good()
        idx = max(i for i, l in enumerate(lines) if l.startswith("| keybind"))
        lines[idx] = "| keybind | a | only |"
        with self.assertRaises(features.LedgerError) as cm:
            self.parse_lines(lines)
        self.assertIn("line %d" % (idx + 1), str(cm.exception))

    def test_unknown_status_reports_line(self):
        lines = self.good()
        idx = max(i for i, l in enumerate(lines) if l.startswith("| keybind"))
        lines[idx] = "| keybind | a |  |  | done |  |"
        with self.assertRaises(features.LedgerError) as cm:
            self.parse_lines(lines)
        self.assertIn("line %d" % (idx + 1), str(cm.exception))
        self.assertIn("done", str(cm.exception))

    def test_missing_baseline(self):
        lines = [l for l in self.good() if not l.startswith("基线")]
        with self.assertRaises(features.LedgerError):
            self.parse_lines(lines)

    def test_duplicate_row(self):
        text = features.render_ledger(META, [row("keybind", "a")])
        text += "| keybind | a |  |  | todo |  |\n"
        with self.assertRaises(features.LedgerError) as cm:
            features.parse_ledger(text)
        self.assertIn("line", str(cm.exception))

    def test_bad_header(self):
        lines = self.good()
        idx = next(i for i, l in enumerate(lines) if l.startswith("| kind"))
        lines[idx] = "| kind | id | foo |"
        with self.assertRaises(features.LedgerError):
            self.parse_lines(lines)


class SeedTest(unittest.TestCase):
    def test_appends_only_new_as_todo(self):
        rows = [row("keybind", "a", "old:1", "ox.rs:2", "aligned", "keep")]
        extracted = {"keybind": ex("a", "b")}
        out = features.seed(rows, extracted)
        self.assertEqual(out[0], rows[0])
        self.assertEqual(out[1], row("keybind", "b", "f/b.ts:3"))
        self.assertEqual(len(rows), 1)

    def test_existing_rows_untouched(self):
        rows = [row("keybind", "a", "old:1", "ox.rs:2", "deviation", "why")]
        snapshot = [dict(r) for r in rows]
        features.seed(rows, {"keybind": ex("a")})
        self.assertEqual(rows, snapshot)

    def test_idempotent(self):
        extracted = {"keybind": ex("a", "b"), "dialog": ex("d")}
        once = features.seed([], extracted)
        twice = features.seed(once, extracted)
        self.assertEqual(once, twice)

    def test_rendered_bytes_stable(self):
        extracted = {"keybind": ex("a", "b"), "dialog": ex("d")}
        once = features.render_ledger(META, features.seed([], extracted))
        _, rows = features.parse_ledger(once)
        twice = features.render_ledger(META, features.seed(rows, extracted))
        self.assertEqual(once, twice)


class CheckTest(unittest.TestCase):
    def test_clean(self):
        rows = [row("keybind", "a")]
        rep = features.check(rows, {"keybind": ex("a")})
        self.assertEqual((rep.missing, rep.removed, rep.invalid), ([], [], []))
        self.assertEqual(features.exit_code(rep), 0)

    def test_missing_fails(self):
        rep = features.check([row("keybind", "a")], {"keybind": ex("a", "b")})
        self.assertEqual(rep.missing, [("keybind", "b")])
        self.assertEqual(features.exit_code(rep), 1)

    def test_removed_does_not_fail(self):
        rep = features.check([row("keybind", "a"), row("keybind", "gone")], {"keybind": ex("a")})
        self.assertEqual(rep.removed, [("keybind", "gone")])
        self.assertEqual(features.exit_code(rep), 0)

    def test_invalid_empty_note_fails(self):
        rows = [
            row("keybind", "a", status="deviation"),
            row("keybind", "b", status="n/a", note="  "),
            row("keybind", "c", status="n/a", note="reason"),
        ]
        rep = features.check(rows, {"keybind": ex("a", "b", "c")})
        self.assertEqual([(k, i) for k, i, _ in rep.invalid], [("keybind", "a"), ("keybind", "b")])
        self.assertEqual(features.exit_code(rep), 1)

    def test_unknown_status_invalid(self):
        rep = features.check([row("keybind", "a", status="weird")], {"keybind": ex("a")})
        self.assertEqual(len(rep.invalid), 1)
        self.assertEqual(features.exit_code(rep), 1)

    def test_manual_kind_not_removed(self):
        rows = [row("part_type", "text", status="n/a", note="手工"), row("cli_option", "--model")]
        rep = features.check(rows, {"keybind": ex("a")})
        self.assertEqual(rep.removed, [])
        self.assertEqual(rep.missing, [("keybind", "a")])

    def test_manual_kind_row_removed_when_kind_extracted(self):
        # a hand-registered row inside an extracted kind is only a hint, never a failure
        rows = [row("sdk_call", "plugin.thing", status="n/a", note="手工")]
        rep = features.check(rows, {"sdk_call": ex("a")})
        self.assertEqual(rep.removed, [("sdk_call", "plugin.thing")])

    def test_counts(self):
        rows = [row("keybind", "a"), row("part_type", "text", status="n/a", note="x")]
        counts = features.kind_counts(rows, {"keybind": ex("a", "b")})
        self.assertEqual(counts, [("keybind", 2, 1), ("part_type", None, 1)])


class DeltaTest(unittest.TestCase):
    def test_set_difference_with_ledger_state(self):
        old = {"keybind": ex("a", "b"), "dialog": ex("d1")}
        new = {"keybind": ex("b", "c"), "dialog": ex("d1", "d2")}
        rows = [row("keybind", "a", status="aligned"), row("dialog", "d2", status="partial")]
        d = features.delta(old, new, rows)
        self.assertEqual([(e["kind"], e["id"]) for e in d.added], [("dialog", "d2"), ("keybind", "c")])
        self.assertEqual([(e["kind"], e["id"]) for e in d.removed], [("keybind", "a")])
        added = {e["id"]: e for e in d.added}
        self.assertEqual(added["d2"]["ledger"]["status"], "partial")
        self.assertIsNone(added["c"]["ledger"])
        self.assertEqual(d.removed[0]["ledger"]["status"], "aligned")
        self.assertEqual(added["c"]["file"], "f/c.ts")

    def test_kind_only_in_one_side(self):
        d = features.delta({}, {"keybind": ex("a")}, [])
        self.assertEqual([(e["kind"], e["id"]) for e in d.added], [("keybind", "a")])
        self.assertEqual(d.removed, [])

    def test_identical(self):
        x = {"keybind": ex("a")}
        d = features.delta(x, x, [])
        self.assertEqual((d.added, d.removed), ([], []))


def upstream_has(*refs):
    if not os.path.isdir(UPSTREAM):
        return False
    for ref in refs:
        r = subprocess.run(
            ["git", "-C", UPSTREAM, "rev-parse", "--verify", "--quiet", ref + "^{commit}"],
            capture_output=True,
        )
        if r.returncode != 0:
            return False
    return True


class IntegrationTest(unittest.TestCase):
    def run_cli(self, *argv):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = features.main(list(argv))
        return code, buf.getvalue()

    def test_seed_check_delta(self):
        if not upstream_has("v2.2.0", "v2.2.3"):
            self.skipTest("upstream opencode with v2.2.0/v2.2.3 not available")
        with tempfile.TemporaryDirectory() as tmp:
            ledger = os.path.join(tmp, "features.md")
            base = ["--ledger", ledger, "--root", UPSTREAM]
            code, _ = self.run_cli(*(base + ["seed", "--ref", "v2.2.3"]))
            self.assertEqual(code, 0)
            with open(ledger, encoding="utf-8") as f:
                meta, rows = features.parse_ledger(f.read())
            self.assertEqual(meta["baseline"], "v2.2.3")
            self.assertGreater(len(rows), 0)
            code, out = self.run_cli(*(base + ["check", "--ref", "v2.2.3"]))
            self.assertEqual(code, 0, out)
            code, out = self.run_cli(*(base + ["delta", "v2.2.0", "v2.2.3"]))
            self.assertEqual(code, 0)
            old = features.extract_at(UPSTREAM, "v2.2.0")
            new = features.extract_at(UPSTREAM, "v2.2.3")
            d = features.delta(old, new, rows)
            self.assertIsInstance(d.added, list)
            self.assertIsInstance(d.removed, list)


if __name__ == "__main__":
    unittest.main()
