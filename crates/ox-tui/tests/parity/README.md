# Cell-level parity tooling

Compares ox-tui against upstream opencode TUI at the terminal-cell level
(char, fg, bg, bold/italic/underline/reverse/strike).

- `capture.py OUT.json ROWS COLS SETTLE [--keys HEX,...] -- cmd...` runs a TUI in a pty
  and writes the final screen grid. Needs `pyte` (`python3 -m venv v && v/bin/pip install pyte`).
  OSC 4/10/11 are answered with the palette upstream's `system` theme was captured under
  (fg `a6b2c0`, bg `1b2738`, ANSI 0-15 from `~/.local/state/opencode/latest/tui/system-theme.json`),
  so ox-tui's own probe resolves the same colours as the gold grids.
- `compare.py GOLD ACTUAL [--max N] [--no-color]` lists differing cells; exit 1 on any diff.
  Blank cells ignore fg (invisible).
- `gold/` holds grids captured from upstream opencode 2.2.3 against an empty ox server,
  cwd `/tmp/parity-cwd`.
- `python3 -m unittest tests/parity/test_compare.py` self-tests the comparer.

## Conversation fixture (tool + reasoning + markdown)

No real model is needed. `fakegw.py` is a deterministic OpenAI-compatible gateway
(port 14193): first turn streams reasoning plus a `read` tool call, the turn after the
tool result streams a markdown answer.

```bash
python3 fakegw.py &                       # fake model gateway
OX_DB_PATH=$TMP/ox2.db OX_LISTEN=127.0.0.1:14192 OX_CARDINAL_URL=http://127.0.0.1:14193 \
  OX_CARDINAL_KEY=x OX_DEFAULT_MODEL=cardinal/fake-1 ox serve &
echo hello > /tmp/parity-cwd/hello.txt
# create a session, POST {"text":"read hello.txt"} to /api/session/<id>/prompt, then capture both TUIs
# against that same session: upstream `opencode --server URL -s <id> /tmp/parity-cwd`,
# `ox --server URL --directory /tmp/parity-cwd --session <id>`.
```

Both TUIs must render the same persisted session: the timing footer (`10ms · tok/s`)
is stored data, so the gold only matches the session it was captured from. Re-capture the
gold whenever the fixture session is recreated. `gold-conv-*.json` shows upstream's layout:
`+ Thought`, `→ Explored: 1 read`, markdown body, `Build · model · time` footer, a bottom
prompt box, and a footer row with cwd and `ctrl+p commands`.

## Known residual diffs (Home, after commit d823295)
- Placeholder text: upstream picks it at random per launch; not comparable.
- Version string (`2.2.3` upstream vs ox server/client version).
- Cursor: ox paints the first prompt cell reversed; upstream uses the terminal's native cursor.
