#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
RUNNER="$SCRIPT_DIR/ox_acp_runner.py"
SCORER="$SCRIPT_DIR/score.py"
TIMEOUT="${OX_SMOKE_TIMEOUT:-120}"
BINARY="${OX_SMOKE_BINARY:-$REPO_ROOT/target/release/ox}"
CONFIG_FILE="${OX_CONFIG_FILE:-}"

if [[ -n "${DEEPSEEK_API_KEY:-}" ]]; then
  # A non-empty key is sufficient provider configuration. Ignore OX_CONFIG_FILE
  # entirely so a stale or unreadable config cannot reject the built-in provider.
  CONFIG_FILE=""
elif [[ -z "$CONFIG_FILE" ]]; then
  echo "error: provider configuration is absent; set DEEPSEEK_API_KEY or OX_CONFIG_FILE" >&2
  exit 2
else
  if [[ ! -f "$CONFIG_FILE" || ! -r "$CONFIG_FILE" ]]; then
    echo "error: OX_CONFIG_FILE does not name a readable file: $CONFIG_FILE" >&2
    exit 2
  fi
  # [[ -r ]] can report success for root even when all read permission bits
  # are clear. Check those bits explicitly, then verify that this shell can
  # open the file without reading or printing its contents.
  CONFIG_MODE="$(stat -f '%Lp' -- "$CONFIG_FILE" 2>/dev/null || stat -c '%a' -- "$CONFIG_FILE" 2>/dev/null || true)"
  if [[ ! "$CONFIG_MODE" =~ ^[0-7]{3,4}$ ]] || (( 8#${CONFIG_MODE: -3} & 0444 == 0 )); then
    echo "error: OX_CONFIG_FILE does not name a readable file: $CONFIG_FILE" >&2
    exit 2
  fi
  if ! exec 9<"$CONFIG_FILE"; then
    echo "error: OX_CONFIG_FILE does not name a readable file: $CONFIG_FILE" >&2
    exit 2
  fi
  exec 9<&-
  CONFIG_FILE="$(cd -- "$(dirname -- "$CONFIG_FILE")" && pwd)/$(basename -- "$CONFIG_FILE")"
fi

if [[ ! -x "$BINARY" ]]; then
  if ! command -v cargo >/dev/null 2>&1; then
    echo "error: release binary is missing at $BINARY and cargo is not installed" >&2
    exit 127
  fi
  echo "release binary not found; building $BINARY" >&2
  if ! cargo build --release --bin ox --manifest-path "$REPO_ROOT/Cargo.toml"; then
    echo "error: release binary is missing at $BINARY and the release build failed" >&2
    exit 127
  fi
fi
if [[ ! -x "$BINARY" ]]; then
  echo "error: release binary is missing or not executable: $BINARY" >&2
  exit 127
fi

if [[ -n "${OX_SMOKE_OUTPUT_DIR:-}" ]]; then
  OUTPUT_DIR="$(mkdir -p -- "$OX_SMOKE_OUTPUT_DIR" && cd -- "$OX_SMOKE_OUTPUT_DIR" && pwd)"
  if [[ -n "$(find "$OUTPUT_DIR" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
    echo "error: smoke output directory is not empty: $OUTPUT_DIR" >&2
    exit 2
  fi
else
  mkdir -p -- "$REPO_ROOT/.ox/local-eval"
  OUTPUT_DIR="$(mktemp -d "$REPO_ROOT/.ox/local-eval/smoke.XXXXXX")"
fi

HOME_DIR="$OUTPUT_DIR/home"
WORKSPACE="$OUTPUT_DIR/workspace"
DB_PATH="$OUTPUT_DIR/ox.db"
CONFIG_DEST=""
mkdir -p -- "$HOME_DIR" "$WORKSPACE"
if [[ -n "$CONFIG_FILE" ]]; then
  mkdir -p -- "$HOME_DIR/.ox"
  CONFIG_DEST="$HOME_DIR/.ox/ox.toml"
  if [[ "$CONFIG_FILE" != "$CONFIG_DEST" ]]; then
    ln -s -- "$CONFIG_FILE" "$CONFIG_DEST"
  fi
fi
cleanup_config() {
  if [[ -n "$CONFIG_DEST" && -L "$CONFIG_DEST" ]]; then
    rm -f -- "$CONFIG_DEST"
  fi
}
trap cleanup_config EXIT
cat > "$WORKSPACE/TASK.md" <<'EOF'
# Local filesystem smoke task

Create `answer.txt` in this directory. Its complete contents must be exactly:

    ox-local-smoke-ok

Do not modify TASK.md.
EOF

PROMPT='Complete the local filesystem smoke task in the current working directory. Read TASK.md, create answer.txt with exactly the requested contents, then read answer.txt and report its exact contents. This is only an ACP/tool-chain smoke test, not a benchmark task; do not claim a benchmark score.'

set +e
HOME="$HOME_DIR" OX_DB_PATH="$DB_PATH" python3 "$RUNNER" \
  --binary "$BINARY" \
  --arg acp \
  --prompt "$PROMPT" \
  --cwd "$WORKSPACE" \
  --output-dir "$OUTPUT_DIR" \
  --timeout "$TIMEOUT"
RUNNER_STATUS=$?
set -e

python3 "$SCORER" "$OUTPUT_DIR" --output "$OUTPUT_DIR/summary.json" | tee "$OUTPUT_DIR/summary.txt"

python3 - "$OUTPUT_DIR/trial.json" "$WORKSPACE/answer.txt" "$OUTPUT_DIR/smoke-result.json" <<'PY'
import json
import sys
from pathlib import Path

trial_path, answer_path, result_path = map(Path, sys.argv[1:])
trial = json.loads(trial_path.read_text(encoding="utf-8"))
expected = "ox-local-smoke-ok\n"
actual = answer_path.read_text(encoding="utf-8") if answer_path.is_file() else None
result = {
    "schema_version": 1,
    "kind": "local_filesystem_smoke",
    "benchmark_pass": False,
    "trial_status": trial.get("status"),
    "verifier_status": trial.get("verifier", {}).get("status"),
    "filesystem_check": "pass" if actual == expected else "fail",
    "answer_file": str(answer_path),
    "answer_matches_expected": actual == expected,
}
result_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
print(json.dumps(result, indent=2, sort_keys=True))
PY

if [[ "$RUNNER_STATUS" -ne 0 ]]; then
  echo "local smoke did not complete; see $OUTPUT_DIR/trial.json and logs" >&2
  exit "$RUNNER_STATUS"
fi
if [[ ! -f "$WORKSPACE/answer.txt" ]] || [[ "$(cat "$WORKSPACE/answer.txt")" != "ox-local-smoke-ok" ]] || [[ "$(wc -l < "$WORKSPACE/answer.txt")" -ne 1 ]]; then
  echo "local smoke filesystem check failed; see $OUTPUT_DIR/smoke-result.json" >&2
  exit 1
fi

echo "local smoke completed; this is not a benchmark pass (verifier remains unverified)"
echo "auditable result: $OUTPUT_DIR/smoke-result.json"
