import contextlib
import copy
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import openapi_diff  # noqa: E402

UPSTREAM = "/Users/john/github/opencode"
SCRIPT = os.path.join(os.path.dirname(HERE), "openapi_diff.py")

EMPTY_KEYS = [
    "paths_added",
    "paths_removed",
    "operations_added",
    "operations_removed",
    "operations_changed",
    "schemas_added",
    "schemas_removed",
    "schemas_changed",
]


def ref(name):
    return {"$ref": "#/components/schemas/%s" % name}


def base():
    return {
        "paths": {
            "/api/x": {
                "get": {
                    "operationId": "x.get",
                    "summary": "Get x",
                    "parameters": [
                        {"in": "query", "name": "q", "required": False, "schema": {"type": "string"}}
                    ],
                    "responses": {
                        "200": {"content": {"application/json": {"schema": ref("X")}}},
                        "404": {"description": "nope"},
                    },
                },
                "post": {
                    "operationId": "x.post",
                    "requestBody": {
                        "required": True,
                        "content": {"application/json": {"schema": ref("XIn")}},
                    },
                    "responses": {"200": {"description": "ok"}},
                },
            },
            "/api/y": {"get": {"operationId": "y.get", "responses": {"200": {"description": "ok"}}}},
        },
        "components": {
            "schemas": {
                "X": {
                    "type": "object",
                    "properties": {"a": {"type": "string"}, "b": {"type": "number"}},
                    "required": ["a"],
                },
                "XIn": {"type": "string", "enum": ["a", "b"]},
            }
        },
    }


def changes_of(result, op):
    for item in result["operations_changed"]:
        if item["op"] == op:
            return item["changes"]
    return None


def joined(changes):
    return "\n".join(changes)


class NoChangeTest(unittest.TestCase):
    def test_identical_is_all_empty(self):
        r = openapi_diff.diff(base(), copy.deepcopy(base()))
        self.assertEqual(sorted(r), sorted(EMPTY_KEYS))
        for k in EMPTY_KEYS:
            self.assertEqual(r[k], [], k)

    def test_empty_documents(self):
        r = openapi_diff.diff({}, {})
        for k in EMPTY_KEYS:
            self.assertEqual(r[k], [], k)


class PathTest(unittest.TestCase):
    def test_path_added_removed_sorted(self):
        new = base()
        del new["paths"]["/api/y"]
        new["paths"]["/api/z"] = {"get": {"responses": {}}}
        new["paths"]["/api/a"] = {"get": {"responses": {}}}
        r = openapi_diff.diff(base(), new)
        self.assertEqual(r["paths_added"], ["/api/a", "/api/z"])
        self.assertEqual(r["paths_removed"], ["/api/y"])
        self.assertEqual(r["operations_added"], ["GET /api/a", "GET /api/z"])
        self.assertEqual(r["operations_removed"], ["GET /api/y"])

    def test_operation_added_removed_on_existing_path(self):
        new = base()
        new["paths"]["/api/x"]["delete"] = {"responses": {}}
        del new["paths"]["/api/x"]["post"]
        r = openapi_diff.diff(base(), new)
        self.assertEqual(r["paths_added"], [])
        self.assertEqual(r["operations_added"], ["DELETE /api/x"])
        self.assertEqual(r["operations_removed"], ["POST /api/x"])

    def test_non_method_keys_ignored(self):
        new = base()
        new["paths"]["/api/x"]["x-note"] = {"a": 1}
        new["paths"]["/api/x"]["summary"] = "path summary"
        r = openapi_diff.diff(base(), new)
        self.assertEqual(r["operations_added"], [])
        self.assertEqual(r["operations_changed"], [])


class OperationFieldsTest(unittest.TestCase):
    def test_operation_id_summary_deprecated(self):
        new = base()
        op = new["paths"]["/api/x"]["get"]
        op["operationId"] = "experimental.x.get"
        op["summary"] = "Fetch x"
        op["deprecated"] = True
        r = openapi_diff.diff(base(), new)
        c = changes_of(r, "GET /api/x")
        self.assertIsNotNone(c)
        text = joined(c)
        self.assertIn("operationId changed: x.get -> experimental.x.get", text)
        self.assertIn("summary changed", text)
        self.assertIn("deprecated changed", text)
        self.assertIsNone(changes_of(r, "POST /api/x"))

    def test_parameter_added_removed(self):
        new = base()
        params = new["paths"]["/api/x"]["get"]["parameters"]
        params.append({"in": "header", "name": "h", "required": True, "schema": {"type": "string"}})
        r = openapi_diff.diff(base(), new)
        self.assertIn("parameter added: header h", joined(changes_of(r, "GET /api/x")))
        r2 = openapi_diff.diff(new, base())
        self.assertIn("parameter removed: header h", joined(changes_of(r2, "GET /api/x")))

    def test_parameter_required_and_schema_changed(self):
        new = base()
        p = new["paths"]["/api/x"]["get"]["parameters"][0]
        p["required"] = True
        p["schema"] = ref("Q")
        r = openapi_diff.diff(base(), new)
        text = joined(changes_of(r, "GET /api/x"))
        self.assertIn("parameter query q: required changed", text)
        self.assertIn("parameter query q: schema changed", text)

    def test_parameter_same_name_different_location_distinct(self):
        old = base()
        new = base()
        new["paths"]["/api/x"]["get"]["parameters"].append(
            {"in": "path", "name": "q", "required": True, "schema": {"type": "string"}}
        )
        r = openapi_diff.diff(old, new)
        self.assertIn("parameter added: path q", joined(changes_of(r, "GET /api/x")))

    def test_path_level_parameters_merged(self):
        old = base()
        new = base()
        new["paths"]["/api/x"]["parameters"] = [
            {"in": "path", "name": "id", "required": True, "schema": {"type": "string"}}
        ]
        r = openapi_diff.diff(old, new)
        # both operations on the path now carry the parameter
        self.assertIn("parameter added: path id", joined(changes_of(r, "GET /api/x")))
        self.assertIn("parameter added: path id", joined(changes_of(r, "POST /api/x")))

    def test_operation_param_overrides_path_param(self):
        old = base()
        old["paths"]["/api/x"]["parameters"] = [
            {"in": "query", "name": "q", "required": True, "schema": {"type": "string"}}
        ]
        new = copy.deepcopy(old)
        # override at op level with required False (op wins) -> equal to old merged? old merged: GET has op's q (required False)
        r = openapi_diff.diff(old, new)
        self.assertEqual(r["operations_changed"], [])
        # changing the path-level one while op overrides: no change for GET
        new["paths"]["/api/x"]["parameters"][0]["required"] = False
        r = openapi_diff.diff(old, new)
        self.assertIsNone(changes_of(r, "GET /api/x"))


class BodyAndResponseTest(unittest.TestCase):
    def test_request_body_required_and_schema(self):
        new = base()
        rb = new["paths"]["/api/x"]["post"]["requestBody"]
        rb["required"] = False
        rb["content"]["application/json"]["schema"] = ref("XIn2")
        r = openapi_diff.diff(base(), new)
        text = joined(changes_of(r, "POST /api/x"))
        self.assertIn("requestBody required changed", text)
        self.assertIn("requestBody application/json: schema changed", text)

    def test_request_body_media_type_added_removed(self):
        new = base()
        new["paths"]["/api/x"]["post"]["requestBody"]["content"]["text/plain"] = {"schema": {"type": "string"}}
        r = openapi_diff.diff(base(), new)
        self.assertIn("requestBody media type added: text/plain", joined(changes_of(r, "POST /api/x")))
        r2 = openapi_diff.diff(new, base())
        self.assertIn("requestBody media type removed: text/plain", joined(changes_of(r2, "POST /api/x")))

    def test_request_body_added_removed(self):
        new = base()
        del new["paths"]["/api/x"]["post"]["requestBody"]
        r = openapi_diff.diff(base(), new)
        self.assertIn("requestBody removed", joined(changes_of(r, "POST /api/x")))
        r2 = openapi_diff.diff(new, base())
        self.assertIn("requestBody added", joined(changes_of(r2, "POST /api/x")))

    def test_response_status_added_removed(self):
        new = base()
        del new["paths"]["/api/x"]["get"]["responses"]["404"]
        new["paths"]["/api/x"]["get"]["responses"]["500"] = {"description": "boom"}
        r = openapi_diff.diff(base(), new)
        text = joined(changes_of(r, "GET /api/x"))
        self.assertIn("response removed: 404", text)
        self.assertIn("response added: 500", text)

    def test_response_schema_ref_changed(self):
        new = base()
        new["paths"]["/api/x"]["get"]["responses"]["200"]["content"]["application/json"]["schema"] = ref("Y")
        r = openapi_diff.diff(base(), new)
        text = joined(changes_of(r, "GET /api/x"))
        self.assertIn("response 200 application/json: schema changed", text)
        self.assertIn("X", text)
        self.assertIn("Y", text)

    def test_inline_schema_changed_described_briefly(self):
        old = base()
        old["paths"]["/api/x"]["get"]["responses"]["200"]["content"]["application/json"]["schema"] = {
            "type": "object",
            "properties": {"a": {"type": "string"}},
        }
        new = copy.deepcopy(old)
        new["paths"]["/api/x"]["get"]["responses"]["200"]["content"]["application/json"]["schema"][
            "properties"
        ]["a"] = {"type": "number"}
        r = openapi_diff.diff(old, new)
        text = joined(changes_of(r, "GET /api/x"))
        self.assertIn("inline schema changed", text)

    def test_response_media_type_added(self):
        new = base()
        new["paths"]["/api/x"]["get"]["responses"]["200"]["content"]["text/plain"] = {"schema": {"type": "string"}}
        r = openapi_diff.diff(base(), new)
        self.assertIn("response 200 media type added: text/plain", joined(changes_of(r, "GET /api/x")))

    def test_equal_inline_schema_no_change(self):
        old = base()
        old["paths"]["/api/x"]["get"]["responses"]["200"]["content"]["application/json"]["schema"] = {
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
        }
        new = json.loads(json.dumps(old))
        r = openapi_diff.diff(old, new)
        self.assertEqual(r["operations_changed"], [])


class SchemaTest(unittest.TestCase):
    def test_schemas_added_removed(self):
        new = base()
        del new["components"]["schemas"]["XIn"]
        new["components"]["schemas"]["Z"] = {"type": "string"}
        new["components"]["schemas"]["A"] = {"type": "string"}
        r = openapi_diff.diff(base(), new)
        self.assertEqual(r["schemas_added"], ["A", "Z"])
        self.assertEqual(r["schemas_removed"], ["XIn"])

    def test_schema_property_and_required_changes(self):
        new = base()
        x = new["components"]["schemas"]["X"]
        del x["properties"]["b"]
        x["properties"]["foo"] = {"type": "string"}
        x["required"] = ["foo"]
        r = openapi_diff.diff(base(), new)
        self.assertEqual(len(r["schemas_changed"]), 1)
        item = r["schemas_changed"][0]
        self.assertEqual(item["schema"], "X")
        self.assertEqual(
            sorted(item["changes"]),
            sorted(
                [
                    "property added: foo",
                    "property removed: b",
                    "required added: foo",
                    "required removed: a",
                ]
            ),
        )

    def test_schema_enum_type_composition_changes(self):
        new = base()
        new["components"]["schemas"]["XIn"]["enum"] = ["a", "b", "c"]
        new["components"]["schemas"]["XIn"]["type"] = "number"
        new["components"]["schemas"]["X"]["oneOf"] = [ref("A")]
        new["components"]["schemas"]["X"]["anyOf"] = [ref("A")]
        new["components"]["schemas"]["X"]["allOf"] = [ref("A")]
        r = openapi_diff.diff(base(), new)
        by = {i["schema"]: i["changes"] for i in r["schemas_changed"]}
        self.assertIn("enum changed", by["XIn"])
        self.assertIn("type changed", by["XIn"])
        for k in ("oneOf", "anyOf", "allOf"):
            self.assertIn("%s changed" % k, by["X"])

    def test_property_type_change_not_reported(self):
        # spec: only property key add/remove is reported
        new = base()
        new["components"]["schemas"]["X"]["properties"]["a"] = {"type": "number"}
        r = openapi_diff.diff(base(), new)
        self.assertEqual(r["schemas_changed"], [])

    def test_output_sorted(self):
        new = base()
        new["components"]["schemas"]["X"]["required"] = ["a", "z"]
        new["components"]["schemas"]["XIn"]["enum"] = ["q"]
        new["paths"]["/api/x"]["get"]["summary"] = "s"
        new["paths"]["/api/x"]["post"]["summary"] = "s"
        r = openapi_diff.diff(base(), new)
        self.assertEqual([i["schema"] for i in r["schemas_changed"]], ["X", "XIn"])
        self.assertEqual([i["op"] for i in r["operations_changed"]], ["GET /api/x", "POST /api/x"])


class FormatTest(unittest.TestCase):
    def test_format_no_change(self):
        r = openapi_diff.diff(base(), base())
        self.assertEqual(openapi_diff.format_text(r).strip(), "无协议层变化")

    def test_format_sections_omit_empty(self):
        new = base()
        new["paths"]["/api/z"] = {"get": {"responses": {}}}
        text = openapi_diff.format_text(openapi_diff.diff(base(), new))
        self.assertIn("paths_added", text)
        self.assertIn("  /api/z", text)
        self.assertNotIn("paths_removed", text)
        self.assertNotIn("schemas_changed", text)


class CliFileTest(unittest.TestCase):
    def run_cli(self, *args):
        env = dict(os.environ)
        env.pop("MallocStackLogging", None)
        env.pop("MallocStackLoggingNoCompact", None)
        return subprocess.run(
            [sys.executable, SCRIPT] + list(args),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            env=env,
        )

    def test_files_json_and_text(self):
        with tempfile.TemporaryDirectory() as d:
            a = os.path.join(d, "a.json")
            b = os.path.join(d, "b.json")
            new = base()
            new["paths"]["/api/z"] = {"get": {"responses": {}}}
            with open(a, "w") as f:
                json.dump(base(), f)
            with open(b, "w") as f:
                json.dump(new, f)
            p = self.run_cli("--old-file", a, "--new-file", b, "--json")
            self.assertEqual(p.returncode, 0, p.stderr)
            self.assertEqual(json.loads(p.stdout)["paths_added"], ["/api/z"])
            p = self.run_cli("--old-file", a, "--new-file", b)
            self.assertEqual(p.returncode, 0, p.stderr)
            self.assertIn("/api/z", p.stdout)
            p = self.run_cli("--old-file", a, "--new-file", a)
            self.assertIn("无协议层变化", p.stdout)

    def test_missing_file_errors(self):
        p = self.run_cli("--old-file", "/nonexistent/a.json", "--new-file", "/nonexistent/b.json")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("a.json", p.stderr)

    def test_bad_tag_errors(self):
        if not os.path.isdir(os.path.join(UPSTREAM, ".git")):
            self.skipTest("upstream repo not present")
        p = self.run_cli("no-such-tag-xyz", "v2.2.1", "--root", UPSTREAM)
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("no-such-tag-xyz", p.stderr)

    def test_missing_args_errors(self):
        p = self.run_cli()
        self.assertNotEqual(p.returncode, 0)


def has_tags(*tags):
    if not os.path.isdir(os.path.join(UPSTREAM, ".git")):
        return False
    for t in tags:
        r = subprocess.run(
            ["git", "-C", UPSTREAM, "rev-parse", "--verify", "-q", t + "^{commit}"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if r.returncode != 0:
            return False
    return True


class UpstreamBaselineTest(unittest.TestCase):
    """v2.2.0 -> v2.2.1 must match .ox/plans/2026-10-29-upstream-2.2.1-delta.md section 1."""

    def test_2_2_0_to_2_2_1(self):
        if not has_tags("v2.2.0", "v2.2.1"):
            self.skipTest("upstream tags not available")
        old = openapi_diff.load_tag(UPSTREAM, "v2.2.0")
        new = openapi_diff.load_tag(UPSTREAM, "v2.2.1")
        r = openapi_diff.diff(old, new)
        self.assertEqual(len(r["paths_added"]), 4, r["paths_added"])
        for p in r["paths_added"]:
            self.assertTrue(p.startswith("/api/experimental/schedule"), p)
        self.assertEqual(r["paths_removed"], [])
        self.assertEqual(len(r["operations_added"]) >= 4, True)
        self.assertEqual(r["operations_removed"], [])
        # doc: existing operations have no parameter/response changes, only operationId prefixes
        for item in r["operations_changed"]:
            for c in item["changes"]:
                self.assertTrue(c.startswith("operationId changed:"), (item["op"], c))
                self.assertIn("-> experimental.", c)
        self.assertEqual(r["schemas_removed"], [])
        self.assertTrue(any(s.startswith("Schedule") for s in r["schemas_added"]))
        self.assertIn("Model.VariantSettings", r["schemas_added"])


if __name__ == "__main__":
    unittest.main()
