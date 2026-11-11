# Local evaluation

This directory contains a small, auditable ACP runner and result aggregator. It
is a local harness, not the FrontierHarness benchmark implementation.

## Prerequisites

- Python 3 (the runner and tests use only the standard library).
- A built ox binary, or Rust/Cargo so `run-smoke.sh` can build
  `target/release/ox`.
- A configured provider for a real model turn.

The smoke script accepts either a non-empty `DEEPSEEK_API_KEY` or an
`OX_CONFIG_FILE` environment variable naming a readable config file:

```bash
export DEEPSEEK_API_KEY='set this in your shell, not in this document'
# Or, for a custom provider configuration:
export OX_CONFIG_FILE="$PWD/ox.toml"
```

`DEEPSEEK_API_KEY` supplies the built-in DeepSeek provider's credential.
`OX_CONFIG_FILE` is a smoke-script input, not an ox setting: the script links
the named file into the isolated trial as `home/.ox/ox.toml`. That file must
contain the provider/model configuration needed by ox and must not be committed
with credentials. If neither variable is usable, the smoke script exits with
status 2 before it checks the binary or creates an output directory/trial
record.

### Supplying a custom gateway

`OX_CONFIG_FILE` must point to a complete, readable ox configuration, not merely
to a file containing a key. For a custom gateway, either use the implicit
`cardinal_url` endpoint:

```toml
# ox-local.toml — use placeholders; do not commit a real key
cardinal_url = "https://gateway.example.invalid"
cardinal_key = "REPLACE_WITH_PROVIDER_KEY"
default_model = "cardinal/kimi-k3-placeholder"
```

or define an explicit provider and model route:

```toml
# ox-local.toml — use placeholders; do not commit a real key
default_model = "kimi-k3"

[providers.kimi]
base_url = "https://gateway.example.invalid"
api_key = "REPLACE_WITH_PROVIDER_KEY"
protocol = "chat_completions" # or "messages", as required by the gateway

[models.kimi-k3]
candidates = [{ provider = "kimi", model = "kimi-k3-placeholder" }]
```

`base_url` is the part before the protocol version suffix: ox appends
`/v1/chat/completions` for `chat_completions` or `/v1/messages` for `messages`.
Use a provider route that matches the gateway's protocol. Kimi K3 is a custom
provider in ox (there is no built-in Kimi provider), so configure it under
`[providers.<name>]` with the gateway's base URL without its version suffix and
the matching protocol. The model and URL above are sanitized placeholders;
replace them with the gateway's actual model id and endpoint in a local file.

For the smoke, pass that file explicitly:

```bash
export OX_CONFIG_FILE="$PWD/ox-local.toml"
./tools/local-eval/run-smoke.sh
```

The script links `OX_CONFIG_FILE` as `home/.ox/ox.toml` inside the trial. The
trial's `HOME` is isolated, so a provider configured only in your own
`~/.ox/ox.toml` is not visible; pass that configuration through
`OX_CONFIG_FILE` instead. Keep the file and its key out of source control.

## ACP runner

Run one prompt from the repository root. The runner does not create `--cwd`,
so create the workspace first; `acp` is an argument to the ox binary:

```bash
WORKSPACE="$PWD/task-workspace"
OUTPUT="$PWD/.ox/local-eval/trial-001"
mkdir -p "$WORKSPACE"
python3 tools/local-eval/ox_acp_runner.py \
  --binary ./target/release/ox \
  --arg acp \
  --prompt 'Complete the task in the current working directory.' \
  --cwd "$WORKSPACE" \
  --output-dir "$OUTPUT" \
  --timeout 120
```

The command is also available as `python3 tools/local-eval/ox_acp_runner.py
--help`. `--arg VALUE` can be repeated for arguments passed to the binary;
`--server-url URL` passes `--server URL` after the supplied arguments. The
runner performs ACP `initialize`, `session/new`, and `session/prompt`. Its
`--timeout` is one wall-clock deadline shared by those requests; a deadline
expiry is recorded as runner status `timeout`, not as infrastructure failure.
The command exits 0 only when the ACP conversation completes, and a successful
ACP conversation is not a task-verifier pass.

Each trial gets an isolated `HOME` and `OX_DB_PATH` under its output directory.
The child does not use the caller's `~/.ox` database or configuration. If a
direct-runner trial needs configuration, create it before invocation at
`<output-dir>/home/.ox/ox.toml`; keep credentials out of logs and source
control. The runner uses the supplied existing `cwd` as the task workspace and
does not copy the repository.

## Trial record and files

The runner writes these paths under the supplied output directory:

- `trial.json`: the structured result;
- `raw.jsonl`: every sent/received ACP or stderr event as it arrives;
- `stdout.log` and `stderr.log`: separated child streams.

`trial.json` currently contains these top-level fields:

- `schema_version`, `runner`, `status`, and `ok`;
- `started_at`, `finished_at`, `wall_time_seconds`, `command`, `cwd`, and
  `timeout_seconds`;
- `environment.HOME` and `environment.OX_DB_PATH` (isolated paths, not
  credentials);
- `protocol`: `version`, request `{id, method}` entries, response and
  notification counts, notification methods, `session_id`, `stop_reason`, and
  protocol `errors`;
- `process.returncode` and `process.terminated_by_runner`;
- `verifier`: initially `{ "status": "unverified", "result": null }`;
- `files.trial`, `files.raw_jsonl`, `files.stdout`, and `files.stderr`;
- `error`, which is null after ACP completion or contains `{kind, message}`.

Runner statuses are `completed`, `timeout`, `protocol_error`, `process_error`,
`spawn_error`, and `infra`. `ok: true` means only that ACP completed; the
runner does not verify task results. It also does not record or infer token
usage or cost: there are no token-usage fields when the ACP/provider does not
supply them.

## Scoring and verifier contract

Aggregate trial files or directories with:

```bash
python3 tools/local-eval/score.py .ox/local-eval \
  --output .ox/local-eval/summary.json
```

See the exact CLI with `python3 tools/local-eval/score.py --help`. The scorer
recognizes these verifier statuses:

- `pass`: task verifier passed;
- `fail`: task verifier ran and rejected the task;
- `timeout`: the verifier or task exceeded its deadline;
- `infra`: the verifier could not run because of infrastructure or an
  unavailable/unauthenticated dependency;
- `incomplete`: no usable verifier outcome exists.

The scorer maps a runner `timeout` to `timeout`. Any other non-`completed`
runner status maps to `infra`; this includes process, protocol, spawn, and
other runner failures. A completed trial with the initial `unverified` value
maps to `incomplete`. Only explicit verifier `pass` and `fail` outcomes form
the `pass / (pass + fail)` denominator.

An external benchmark/verifier should run after the agent, inspect the task
workspace and any required task command output, then write an explicit outcome
into that task's `trial.json` without replacing the runner fields. For example:

```python
import json
from pathlib import Path

path = Path(".ox/local-eval/trial-001/trial.json")
trial = json.loads(path.read_text())
trial["verifier"] = {
    "status": "pass",  # pass|fail|timeout|infra|incomplete
    "result": {"checker": "external-checker", "details": "..."},
}
path.write_text(json.dumps(trial, indent=2, sort_keys=True) + "\n")
```

The benchmark adapter owns the verifier's `result` shape; its `status` must
follow the contract above. Give every benchmark task its own existing `cwd`
and output directory, run the runner, invoke the official/external verifier,
annotate the record, and then point `score.py` at the collection. This
repository does not bundle the official FrontierHarness task images or
verifier commands, so do not invent them here.

## Fixture tests and smoke

Run the standard-library fixture tests:

```bash
python3 -m unittest discover -s tools/local-eval -p 'test_*.py' -v
```

Run the filesystem smoke task (the output directory must be empty if supplied):

```bash
./tools/local-eval/run-smoke.sh
```

To use a config file instead, set `OX_CONFIG_FILE` as described above. Useful
overrides are `OX_SMOKE_BINARY`, `OX_SMOKE_TIMEOUT`, and
`OX_SMOKE_OUTPUT_DIR`. Without an output override, results are placed in a
fresh `.ox/local-eval/smoke.*` directory. The script creates `home/`,
`ox.db`, and `workspace/`; writes `workspace/TASK.md`; runs the runner; and
writes `trial.json`, `raw.jsonl`, `stdout.log`, `stderr.log`, `summary.json`,
`summary.txt`, and `smoke-result.json` in that output directory. The task asks
the agent to create `workspace/answer.txt`.

The filesystem check can pass while the scorer reports `incomplete`, because
the smoke deliberately has no task verifier. Its final line explicitly says
that it is not a benchmark pass. If a provider precheck fails, the script exits
before the runner and no trial record is produced.

## Limitations and result interpretation

- Local smoke is an ACP, workspace, and tool-execution check; it is **not** a
  FrontierHarness benchmark score and must not be reported as one.
- No verifier means `incomplete`, not pass. Do not include it in pass-rate
  coverage.
- A provider that is unavailable or unauthenticated, Docker unavailable,
  missing task images, or a verifier that cannot start are `infra`, not pass.
  Preserve the trial and logs so the missing coverage is auditable.
- The aggregator reports valid coverage separately from total trials. A score
  with unrun or infrastructure-blocked tasks omitted is not a complete
  benchmark result.
