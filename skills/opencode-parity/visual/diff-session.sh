#!/usr/bin/env bash
# Capture upstream opencode and ox-tui rendering the same session, then diff cell by cell.
#
# usage: diff-session.sh SESSION_ID [ROWS] [COLS] [KEYS_HEX]
#
# env:
#   SERVER    server both TUIs attach to   (default http://127.0.0.1:14192)
#   CWD       project directory            (default /tmp/parity-cwd)
#   OPENCODE  upstream binary              (default ~/.opencode/bin/opencode)
#   OX        ox binary                    (default ox on PATH)
#   SETTLE    seconds to wait before grab  (default 4)
#   OUT       output directory             (default ~/.ox/tmp/parity)
#   PY        python with pyte installed   (default ./v/bin/python, else python3)
# Extra compare.py flags can follow `--`, e.g. `diff-session.sh ID 30 100 '' -- --no-color`.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
sid="${1:?usage: diff-session.sh SESSION_ID [ROWS] [COLS] [KEYS_HEX] [-- compare flags]}"
shift
rows=30 cols=100 keys=""
[[ $# -gt 0 && $1 != "--" ]] && { rows=$1; shift; }
[[ $# -gt 0 && $1 != "--" ]] && { cols=$1; shift; }
[[ $# -gt 0 && $1 != "--" ]] && { keys=$1; shift; }
[[ $# -gt 0 && $1 == "--" ]] && shift

server="${SERVER:-http://127.0.0.1:14192}"
cwd="${CWD:-/tmp/parity-cwd}"
opencode="${OPENCODE:-$HOME/.opencode/bin/opencode}"
ox="${OX:-ox}"
settle="${SETTLE:-4}"
out="${OUT:-$HOME/.ox/tmp/parity}"
py="${PY:-}"
if [[ -z $py ]]; then
  if [[ -x $here/v/bin/python ]]; then py="$here/v/bin/python"; else py=python3; fi
fi

mkdir -p "$out" "$cwd"
up="$out/upstream-$sid-${rows}x$cols.json"
mine="$out/ox-$sid-${rows}x$cols.json"
keyargs=()
[[ -n $keys ]] && keyargs=(--keys "$keys")

echo "== upstream ($up)"
"$py" "$here/capture.py" "$up" "$rows" "$cols" "$settle" ${keyargs[@]+"${keyargs[@]}"} -- \
  "$opencode" --server "$server" -s "$sid" "$cwd"
echo "== ox ($mine)"
"$py" "$here/capture.py" "$mine" "$rows" "$cols" "$settle" ${keyargs[@]+"${keyargs[@]}"} -- \
  "$ox" --server "$server" --directory "$cwd" --session "$sid"
echo "== diff (gold = upstream)"
exec "$py" "$here/compare.py" "$up" "$mine" "$@"
