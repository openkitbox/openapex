#!/usr/bin/env python3
"""Run a single ACP v1 prompt and record an auditable local trial."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import sys
import threading
import time
from typing import Any, Sequence


class RunnerError(Exception):
    """A protocol or process failure with a stable trial error kind."""

    def __init__(self, kind: str, message: str) -> None:
        super().__init__(message)
        self.kind = kind
        self.message = message


def _timestamp() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def _read_stream(name: str, stream: Any, events: queue.Queue[tuple[str, bytes | None]]) -> None:
    try:
        for line in iter(stream.readline, b""):
            events.put((name, line))
    finally:
        events.put((name, None))
        stream.close()


def _request(request_id: int, method: str, params: dict[str, Any]) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}


def _record_event(
    raw_file: Any,
    *,
    direction: str,
    channel: str,
    value: str,
    parsed: Any | None = None,
) -> None:
    record: dict[str, Any] = {
        "timestamp": _timestamp(),
        "direction": direction,
        "channel": channel,
        "raw": value,
    }
    if parsed is not None:
        record["message"] = parsed
    raw_file.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")
    raw_file.flush()


def _send(
    process: subprocess.Popen[bytes],
    raw_file: Any,
    request_id: int,
    method: str,
    params: dict[str, Any],
    requests: list[dict[str, Any]],
) -> None:
    message = _request(request_id, method, params)
    encoded = (json.dumps(message, separators=(",", ":")) + "\n").encode()
    try:
        process.stdin.write(encoded)
        process.stdin.flush()
    except (BrokenPipeError, OSError) as error:
        raise RunnerError("process_error", f"failed to send {method}: {error}") from error
    requests.append({"id": request_id, "method": method})
    _record_event(
        raw_file,
        direction="send",
        channel="stdin",
        value=encoded.decode().rstrip("\n"),
        parsed=message,
    )


def _protocol_error(protocol: dict[str, Any], kind: str, message: str, **details: Any) -> None:
    error: dict[str, Any] = {"kind": kind, "message": message}
    error.update(details)
    protocol["errors"].append(error)
    raise RunnerError("protocol_error", message)


def _wait_for_response(
    process: subprocess.Popen[bytes],
    events: queue.Queue[tuple[str, bytes | None]],
    raw_file: Any,
    stdout_file: Any,
    stderr_file: Any,
    request_id: int,
    deadline: float,
    protocol: dict[str, Any],
    stream_done: set[str],
) -> dict[str, Any]:
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RunnerError("timeout", "ACP request timed out")
        try:
            channel, line = events.get(timeout=remaining)
        except queue.Empty as error:
            raise RunnerError("timeout", "ACP request timed out") from error

        if line is None:
            stream_done.add(channel)
            if channel == "stdout":
                try:
                    process.wait(timeout=min(remaining, 1.0))
                except subprocess.TimeoutExpired:
                    pass
                if process.returncode is not None:
                    raise RunnerError("process_error", "ACP child exited before its response")
            continue
        text = line.decode("utf-8", errors="replace").rstrip("\r\n")
        target = stdout_file if channel == "stdout" else stderr_file
        target.write(text + "\n")
        target.flush()
        if channel == "stderr":
            _record_event(raw_file, direction="receive", channel=channel, value=text)
            continue

        _record_event(raw_file, direction="receive", channel=channel, value=text)
        try:
            message = json.loads(text)
        except json.JSONDecodeError as error:
            protocol["errors"].append({"kind": "bad_json", "message": str(error), "raw": text})
            raise RunnerError("protocol_error", f"ACP child emitted invalid JSON: {error.msg}") from error
        if not isinstance(message, dict):
            _protocol_error(protocol, "invalid_message", "ACP child emitted a non-object JSON message")
        if "method" in message and "id" not in message:
            if message.get("jsonrpc") != "2.0":
                _protocol_error(protocol, "invalid_notification", "ACP notification did not have JSON-RPC version 2.0")
            if not isinstance(message["method"], str):
                _protocol_error(protocol, "invalid_notification", "ACP notification method was not a string")
            if "params" in message and not isinstance(message["params"], dict):
                _protocol_error(protocol, "invalid_notification", "ACP notification params was not an object")
            protocol["notifications"] += 1
            protocol["notification_methods"].append(message["method"])
            continue
        if "method" in message:
            _protocol_error(protocol, "invalid_response", "ACP response unexpectedly contained a method")
        if message.get("jsonrpc") != "2.0":
            _protocol_error(protocol, "invalid_response", "ACP response did not have JSON-RPC version 2.0")
        if "id" not in message or message["id"] != request_id or isinstance(message["id"], bool):
            _protocol_error(
                protocol,
                "unexpected_id",
                f"ACP response id did not match {request_id}",
                expected=request_id,
                received=message.get("id"),
            )
        has_result = "result" in message
        has_error = "error" in message
        if has_result == has_error:
            _protocol_error(protocol, "invalid_response", "ACP response must contain exactly one of result or error")
        protocol["responses"] += 1
        if has_error:
            error = message["error"]
            if not isinstance(error, dict) or not isinstance(error.get("code"), int) or not isinstance(
                error.get("message"), str
            ):
                _protocol_error(protocol, "invalid_error", "ACP response error object was malformed")
            protocol["errors"].append({"kind": "rpc_error", "error": error})
            raise RunnerError("protocol_error", f"ACP request failed: {error}")
        if not isinstance(message["result"], dict):
            _protocol_error(protocol, "invalid_result", "ACP response result was not an object")
        return message


def _terminate_process_group(process: subprocess.Popen[bytes], process_group_id: int) -> bool:
    """Terminate a child and all descendants, using a portable fallback."""
    if os.name == "posix":
        try:
            os.killpg(process_group_id, signal.SIGTERM)
            return True
        except ProcessLookupError:
            return False
        except OSError:
            process.terminate()
            return True
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/PID", str(process_group_id), "/T", "/F"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return process.poll() is None
    if process.poll() is None:
        process.terminate()
        return True
    return False


def _kill_process_group(process: subprocess.Popen[bytes], process_group_id: int) -> None:
    if os.name == "posix":
        try:
            os.killpg(process_group_id, signal.SIGKILL)
            return
        except ProcessLookupError:
            return
        except OSError:
            pass
    elif os.name == "nt":
        subprocess.run(
            ["taskkill", "/PID", str(process_group_id), "/T", "/F"],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return
    if process.poll() is None:
        process.kill()


def _process_group_exists(process: subprocess.Popen[bytes], process_group_id: int) -> bool:
    if os.name == "posix":
        try:
            os.killpg(process_group_id, 0)
            return True
        except ProcessLookupError:
            return False
        except OSError:
            return True
    return process.poll() is None


def run_trial(
    command: Sequence[str] | str,
    *,
    prompt: str,
    cwd: str | os.PathLike[str],
    output_dir: str | os.PathLike[str],
    timeout: float = 120.0,
    env: dict[str, str] | None = None,
    server_url: str | None = None,
) -> dict[str, Any]:
    """Run initialize, session/new, and session/prompt against one child.

    The returned dictionary is also written to ``trial.json``. Protocol records
    are written as they arrive to ``raw.jsonl``; stdout and stderr remain in
    separate files and are never treated as protocol input interchangeably.
    """
    if isinstance(command, str):
        command = [command]
    command = list(command)
    if not command:
        raise ValueError("command must not be empty")
    if timeout <= 0:
        raise ValueError("timeout must be positive")

    destination = Path(output_dir)
    destination.mkdir(parents=True, exist_ok=True)
    trial_path = destination / "trial.json"
    raw_path = destination / "raw.jsonl"
    stdout_path = destination / "stdout.log"
    stderr_path = destination / "stderr.log"
    started = time.monotonic()
    started_at = _timestamp()
    protocol: dict[str, Any] = {
        "version": 1,
        "requests": [],
        "responses": 0,
        "notifications": 0,
        "notification_methods": [],
        "session_id": None,
        "stop_reason": None,
        "errors": [],
    }
    result: dict[str, Any] = {
        "schema_version": 1,
        "runner": "ox_acp_runner",
        "status": "infra",
        "ok": False,
        "started_at": started_at,
        "finished_at": None,
        "wall_time_seconds": None,
        "command": command,
        "cwd": str(Path(cwd).resolve()),
        "timeout_seconds": timeout,
        "environment": {
            "HOME": str(destination / "home"),
            "OX_DB_PATH": str(destination / "ox.db"),
        },
        "protocol": protocol,
        "process": {"returncode": None, "terminated_by_runner": False},
        "verifier": {"status": "unverified", "result": None},
        "files": {
            "trial": str(trial_path),
            "raw_jsonl": str(raw_path),
            "stdout": str(stdout_path),
            "stderr": str(stderr_path),
        },
        "error": None,
    }
    process: subprocess.Popen[bytes] | None = None
    process_group_id: int | None = None
    events: queue.Queue[tuple[str, bytes | None]] = queue.Queue()
    threads: list[threading.Thread] = []
    stream_done: set[str] = set()
    raw_file = raw_path.open("w", encoding="utf-8")
    stdout_file = stdout_path.open("w", encoding="utf-8")
    stderr_file = stderr_path.open("w", encoding="utf-8")
    try:
        child_command = command + (["--server", server_url] if server_url else [])
        child_env = os.environ.copy()
        if env is not None:
            child_env.update(env)
        isolated_home = destination / "home"
        isolated_home.mkdir(exist_ok=True)
        child_env["HOME"] = str(isolated_home)
        child_env["OX_DB_PATH"] = str(destination / "ox.db")
        result["environment"] = {"HOME": str(isolated_home), "OX_DB_PATH": str(destination / "ox.db")}
        process_options: dict[str, Any] = {
            "cwd": str(cwd),
            "env": child_env,
            "stdin": subprocess.PIPE,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.PIPE,
        }
        if os.name == "posix":
            process_options["start_new_session"] = True
        elif os.name == "nt":
            process_options["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        process = subprocess.Popen(child_command, **process_options)
        process_group_id = process.pid
        for name, stream in (("stdout", process.stdout), ("stderr", process.stderr)):
            if stream is None:
                raise RunnerError("spawn_error", f"child {name} pipe was not created")
            thread = threading.Thread(target=_read_stream, args=(name, stream, events), daemon=True)
            thread.start()
            threads.append(thread)

        if process.stdin is None:
            raise RunnerError("spawn_error", "child stdin pipe was not created")
        deadline = started + timeout
        _send(
            process,
            raw_file,
            1,
            "initialize",
            {
                "protocolVersion": 1,
                "clientInfo": {"name": "ox-local-eval", "version": "1"},
                "clientCapabilities": {},
            },
            protocol["requests"],
        )
        initialize_response = _wait_for_response(
            process, events, raw_file, stdout_file, stderr_file, 1, deadline, protocol, stream_done
        )
        protocol_version = initialize_response.get("result", {}).get("protocolVersion")
        if isinstance(protocol_version, bool) or not isinstance(protocol_version, int) or protocol_version != 1:
            raise RunnerError("protocol_error", "initialize response did not select ACP protocol version 1")
        _send(
            process,
            raw_file,
            2,
            "session/new",
            {"cwd": str(Path(cwd).resolve()), "mcpServers": []},
            protocol["requests"],
        )
        new_response = _wait_for_response(
            process, events, raw_file, stdout_file, stderr_file, 2, deadline, protocol, stream_done
        )
        session_id = new_response.get("result", {}).get("sessionId")
        if not isinstance(session_id, str) or not session_id:
            raise RunnerError("protocol_error", "session/new response carried no sessionId")
        protocol["session_id"] = session_id
        _send(
            process,
            raw_file,
            3,
            "session/prompt",
            {"sessionId": session_id, "prompt": [{"type": "text", "text": prompt}]},
            protocol["requests"],
        )
        prompt_response = _wait_for_response(
            process, events, raw_file, stdout_file, stderr_file, 3, deadline, protocol, stream_done
        )
        if process.stdin is not None:
            process.stdin.close()
        try:
            process.wait(timeout=min(max(deadline - time.monotonic(), 0.0), 1.0))
        except subprocess.TimeoutExpired:
            pass
        if process.returncode is not None and process.returncode != 0:
            raise RunnerError("process_error", f"ACP child exited with status {process.returncode}")
        stop_reason = prompt_response.get("result", {}).get("stopReason")
        protocol["stop_reason"] = stop_reason
        if not isinstance(stop_reason, str) or stop_reason not in {
            "end_turn",
            "max_tokens",
            "max_turn_requests",
            "refusal",
            "cancelled",
        }:
            raise RunnerError("protocol_error", "session/prompt response carried an invalid stopReason")
        result["status"] = "completed"
        result["ok"] = True
    except RunnerError as error:
        result["status"] = error.kind
        result["error"] = {"kind": error.kind, "message": error.message}
    except (OSError, ValueError) as error:
        result["status"] = "infra"
        result["error"] = {"kind": "spawn_error", "message": str(error)}
    finally:
        if process is not None:
            if process.stdin is not None:
                process.stdin.close()
            terminated = False
            if process_group_id is not None:
                terminated = _terminate_process_group(process, process_group_id)
            elif process.poll() is None:
                process.terminate()
                terminated = True
            if terminated:
                result["process"]["terminated_by_runner"] = True
            grace_deadline = time.monotonic() + 0.5
            while _process_group_exists(process, process_group_id) and time.monotonic() < grace_deadline:
                try:
                    process.wait(timeout=min(grace_deadline - time.monotonic(), 0.05))
                except subprocess.TimeoutExpired:
                    pass
            if _process_group_exists(process, process_group_id):
                if process_group_id is not None:
                    _kill_process_group(process, process_group_id)
                else:
                    process.kill()
            process.wait()
            result["process"]["returncode"] = process.returncode
        for thread in threads:
            thread.join(timeout=2)
        result["finished_at"] = _timestamp()
        result["wall_time_seconds"] = round(time.monotonic() - started, 6)
        raw_file.close()
        stdout_file.close()
        stderr_file.close()
        trial_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return result


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, help="ACP agent executable")
    parser.add_argument("--arg", action="append", default=[], help="argument passed to the binary (repeatable)")
    parser.add_argument("--prompt", required=True)
    parser.add_argument("--cwd", default=os.getcwd())
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--server-url")
    args = parser.parse_args(argv)
    trial = run_trial(
        [args.binary, *args.arg],
        prompt=args.prompt,
        cwd=args.cwd,
        output_dir=args.output_dir,
        timeout=args.timeout,
        server_url=args.server_url,
    )
    print(json.dumps(trial, indent=2, sort_keys=True))
    return 0 if trial["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
