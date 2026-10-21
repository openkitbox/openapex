# ox-tui

Native ratatui client for the ox server (opencode v2 protocol).

## Run

```bash
cargo run -p ox-tui -- --server http://127.0.0.1:14090 --directory "$PWD" [--session ses_…]
```

- `--server` default: `http://127.0.0.1:14090`
- `--directory` default: current directory (sent as `x-opencode-directory`)
- `--session` default: create a new session for the directory

## Keys

| Key | Action |
|---|---|
| ⏎ | send (composer) |
| ⇧⏎ | newline |
| PgUp / PgDn | scroll timeline |
| End | jump to bottom (sticky) |
| Ctrl+C | interrupt while running, quit when idle |

## Layout

Single-column instrumented timeline + composer + one-line dock.
No sidebar, no cursor: history is events, current state lives in the dock.

## Demo

```bash
OX_TUI_DEMO=crates/ox-tui/fixtures/mvp-demo.jsonl cargo run -p ox-tui
```

## Tests

```bash
cargo test -p ox-client
cargo test -p ox-tui
```
