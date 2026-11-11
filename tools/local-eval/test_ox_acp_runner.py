import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import ox_acp_runner


FAKE_AGENT = r'''
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    if request["method"] == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": 1}}), flush=True)
    elif request["method"] == "session/new":
        print(json.dumps({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update": {"sessionUpdate": "agent_message_chunk"}}}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"sessionId": "s-1"}}), flush=True)
    elif request["method"] == "session/prompt":
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"stopReason": "end_turn"}}), flush=True)
        break
'''

BAD_JSON_AGENT = 'import sys\nprint("not json", flush=True)\n'
EXITING_AGENT = 'import sys\nsys.exit(7)\n'
HANGING_AGENT = 'import sys\nfor line in sys.stdin:\n    pass\n'
BAD_JSONRPC_AGENT = (
    'import json, sys\n'
    'for line in sys.stdin:\n'
    '    request = json.loads(line)\n'
    '    print(json.dumps({"jsonrpc": "1.0", "id": request["id"], "result": {}}), flush=True)\n'
)
MALFORMED_NOTIFICATION_AGENT = (
    'import json, sys\n'
    'request = json.loads(sys.stdin.readline())\n'
    'print(json.dumps({"jsonrpc": "1.0", "method": "session/update", "params": {}}), flush=True)\n'
    'print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": 1}}), flush=True)\n'
)
WRONG_ID_AGENT = (
    'import json, sys\n'
    'request = json.loads(sys.stdin.readline())\n'
    'print(json.dumps({"jsonrpc": "2.0", "id": request["id"] + 1, "result": {}}), flush=True)\n'
)
MALFORMED_RESULT_AGENT = (
    'import json, sys\n'
    'for line in sys.stdin:\n'
    '    request = json.loads(line)\n'
    '    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": []}), flush=True)\n'
)
MALFORMED_SESSION_ID_AGENT = (
    'import json, sys\n'
    'for line in sys.stdin:\n'
    '    request = json.loads(line)\n'
    '    result = {"protocolVersion": 1} if request["method"] == "initialize" else {"sessionId": 3}\n'
    '    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)\n'
)
MALFORMED_STOP_REASON_AGENT = (
    'import json, sys\n'
    'for line in sys.stdin:\n'
    '    request = json.loads(line)\n'
    '    if request["method"] == "initialize": result = {"protocolVersion": 1}\n'
    '    elif request["method"] == "session/new": result = {"sessionId": "s-1"}\n'
    '    else: result = {"stopReason": 3}\n'
    '    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)\n'
)
UNSUPPORTED_PROTOCOL_VERSION_AGENT = (
    'import json, sys\n'
    'for line in sys.stdin:\n'
    '    request = json.loads(line)\n'
    '    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": 2}}), flush=True)\n'
)
MALFORMED_PROTOCOL_VERSION_AGENT = (
    'import json, sys\n'
    'for line in sys.stdin:\n'
    '    request = json.loads(line)\n'
    '    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": "1"}}), flush=True)\n'
)
UNKNOWN_STOP_REASON_AGENT = (
    'import json, sys\n'
    'for line in sys.stdin:\n'
    '    request = json.loads(line)\n'
    '    if request["method"] == "initialize": result = {"protocolVersion": 1}\n'
    '    elif request["method"] == "session/new": result = {"sessionId": "s-1"}\n'
    '    else: result = {"stopReason": "unknown"}\n'
    '    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)\n'
)
MISSING_STOP_REASON_AGENT = (
    'import json, sys\n'
    'for line in sys.stdin:\n'
    '    request = json.loads(line)\n'
    '    if request["method"] == "initialize": result = {"protocolVersion": 1}\n'
    '    elif request["method"] == "session/new": result = {"sessionId": "s-1"}\n'
    '    else: result = {}\n'
    '    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)\n'
)
ENVIRONMENT_AGENT = r'''
import json
import os
import pathlib
import sys

pathlib.Path(sys.argv[1]).write_text(json.dumps({
    "cwd": os.getcwd(),
    "HOME": os.environ.get("HOME"),
    "OX_DB_PATH": os.environ.get("OX_DB_PATH"),
}), encoding="utf-8")
for line in sys.stdin:
    request = json.loads(line)
    if request["method"] == "initialize":
        result = {"protocolVersion": 1}
    elif request["method"] == "session/new":
        result = {"sessionId": "s-1"}
    else:
        result = {"stopReason": "end_turn"}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
    if request["method"] == "session/prompt":
        break
'''
DESCENDANT_AGENT = r'''
import json
import pathlib
import subprocess
import sys

request = json.loads(sys.stdin.readline())
path = pathlib.Path(sys.argv[1])
descendant = subprocess.Popen([
    sys.executable, "-c", "import pathlib, sys, time; time.sleep(2); pathlib.Path(sys.argv[1]).write_text('alive')",
    str(path.with_name("descendant-alive")),
])
path.write_text(str(descendant.pid))
print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": 1}}), flush=True)
for line in sys.stdin:
    pass
'''


class OxAcpRunnerTest(unittest.TestCase):
    def run_script(self, source, timeout=5):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            agent = root / "fake_agent.py"
            agent.write_text(source, encoding="utf-8")
            return ox_acp_runner.run_trial(
                [sys.executable, "-u", str(agent)],
                prompt="hello",
                cwd=root,
                output_dir=root / "trial",
                timeout=timeout,
            )

    def test_successful_initialize_new_session_prompt_protocol(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            agent = root / "fake_agent.py"
            agent.write_text(FAKE_AGENT, encoding="utf-8")
            output_dir = root / "trial"

            trial = ox_acp_runner.run_trial(
                [sys.executable, "-u", str(agent)],
                prompt="hello",
                cwd=root,
                output_dir=output_dir,
                timeout=5,
            )

            self.assertEqual(trial["status"], "completed")
            self.assertEqual(trial["protocol"]["session_id"], "s-1")
            self.assertEqual(trial["protocol"]["responses"], 3)
            self.assertEqual(trial["protocol"]["notifications"], 1)
            self.assertEqual(trial["verifier"]["status"], "unverified")
            self.assertEqual(trial["process"]["returncode"], 0)
            self.assertTrue(Path(trial["environment"]["HOME"]).is_dir())
            raw_lines = (output_dir / "raw.jsonl").read_text(encoding="utf-8").splitlines()
            self.assertGreaterEqual(len(raw_lines), 7)
            self.assertTrue(all(json.loads(line)["direction"] in {"send", "receive"} for line in raw_lines))
            self.assertTrue((output_dir / "stderr.log").exists())

    def test_bad_json_is_protocol_error(self):
        trial = self.run_script(BAD_JSON_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")
        self.assertEqual(trial["protocol"]["errors"][0]["kind"], "bad_json")

    def test_invalid_jsonrpc_response_is_protocol_error(self):
        trial = self.run_script(BAD_JSONRPC_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_malformed_notification_is_protocol_error(self):
        trial = self.run_script(MALFORMED_NOTIFICATION_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")
        self.assertEqual(trial["protocol"]["errors"][0]["kind"], "invalid_notification")

    def test_child_observes_cwd_home_and_database_path(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            agent = root / "fake_agent.py"
            observed_path = root / "observed.json"
            agent.write_text(ENVIRONMENT_AGENT, encoding="utf-8")
            output_dir = root / "trial"

            trial = ox_acp_runner.run_trial(
                [sys.executable, "-u", str(agent), str(observed_path)],
                prompt="hello",
                cwd=root,
                output_dir=output_dir,
                timeout=5,
            )

            self.assertEqual(trial["status"], "completed")
            observed = json.loads(observed_path.read_text(encoding="utf-8"))
            self.assertEqual(Path(observed["cwd"]).resolve(), root.resolve())
            self.assertEqual(observed["HOME"], str(output_dir / "home"))
            self.assertEqual(observed["OX_DB_PATH"], str(output_dir / "ox.db"))

    def test_mismatched_response_id_is_protocol_error(self):
        trial = self.run_script(WRONG_ID_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_malformed_result_is_protocol_error(self):
        trial = self.run_script(MALFORMED_RESULT_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_malformed_session_id_is_protocol_error(self):
        trial = self.run_script(MALFORMED_SESSION_ID_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_malformed_stop_reason_is_protocol_error(self):
        trial = self.run_script(MALFORMED_STOP_REASON_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_unsupported_initialize_protocol_version_is_protocol_error(self):
        trial = self.run_script(UNSUPPORTED_PROTOCOL_VERSION_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_malformed_initialize_protocol_version_is_protocol_error(self):
        trial = self.run_script(MALFORMED_PROTOCOL_VERSION_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_unknown_stop_reason_is_protocol_error(self):
        trial = self.run_script(UNKNOWN_STOP_REASON_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_missing_stop_reason_is_protocol_error(self):
        trial = self.run_script(MISSING_STOP_REASON_AGENT)
        self.assertEqual(trial["status"], "protocol_error")
        self.assertEqual(trial["error"]["kind"], "protocol_error")

    def test_nonzero_exit_is_process_error(self):
        trial = self.run_script(EXITING_AGENT)
        self.assertEqual(trial["status"], "process_error")
        self.assertEqual(trial["process"]["returncode"], 7)

    def test_timeout_is_recorded(self):
        trial = self.run_script(HANGING_AGENT, timeout=0.1)
        self.assertEqual(trial["status"], "timeout")
        self.assertEqual(trial["error"]["kind"], "timeout")

    @unittest.skipUnless(os.name == "posix", "process groups are tested on POSIX")
    def test_timeout_kills_descendant_process_group(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            agent = root / "fake_agent.py"
            descendant_pid = root / "descendant.pid"
            alive_marker = root / "descendant-alive"
            agent.write_text(DESCENDANT_AGENT, encoding="utf-8")
            trial = ox_acp_runner.run_trial(
                [sys.executable, "-u", str(agent), str(descendant_pid)],
                prompt="hello",
                cwd=root,
                output_dir=root / "trial",
                timeout=0.5,
            )
            self.assertEqual(trial["status"], "timeout")
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline and not descendant_pid.exists():
                time.sleep(0.01)
            self.assertTrue(descendant_pid.exists())
            pid = int(descendant_pid.read_text(encoding="utf-8"))
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                self.fail("descendant process survived runner cleanup")
            self.assertFalse(alive_marker.exists())

    @unittest.skipUnless(os.name == "posix", "process groups are tested on POSIX")
    def test_timeout_kills_sigterm_ignoring_descendant(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            agent = root / "fake_agent.py"
            descendant_pid = root / "descendant.pid"
            alive_marker = root / "descendant-alive"
            agent.write_text(
                DESCENDANT_AGENT.replace(
                    "import pathlib, sys, time; time.sleep(2)",
                    "import pathlib, signal, sys, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(2)",
                ),
                encoding="utf-8",
            )
            trial = ox_acp_runner.run_trial(
                [sys.executable, "-u", str(agent), str(descendant_pid)],
                prompt="hello",
                cwd=root,
                output_dir=root / "trial",
                timeout=0.5,
            )
            self.assertEqual(trial["status"], "timeout")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline and not descendant_pid.exists():
                time.sleep(0.01)
            self.assertTrue(descendant_pid.exists())
            pid = int(descendant_pid.read_text(encoding="utf-8"))
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                self.fail("SIGTERM-ignoring descendant survived runner cleanup")
            self.assertFalse(alive_marker.exists())


if __name__ == "__main__":
    unittest.main()
