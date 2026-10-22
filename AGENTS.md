# ox

## Scope

- **ox-tui is paused.** The terminal UI (`crates/ox-tui`, plus TUI-only work in
  `ox-client`'s store/render layers) is more work than there is capacity for
  right now. Do not plan, design, implement, review, or estimate TUI work, and
  do not count it as a deliverable or a trade-off in any analysis — unless the
  user explicitly asks about ox-tui in that particular message.
- Everything else stays in scope, including `ox-schema` and `ox-client`'s
  HTTP/SSE protocol surface (the opencode wire-compat anchor) and the server.
