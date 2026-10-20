# ox-todo — opencode TUI sidebar plugin

Renders ox's two-level todo list (phase/task, with `blocked` + blocker note) in
opencode's sidebar, from the structured `metadata.phases` the ox `todo` tool
publishes. Plan: `.opencode/plans/ox-todo-metadata.md`.

## Install

Symlink (or copy) this directory into a plugin directory the TUI scans:

```sh
ln -s /path/to/ox/plugins/ox-todo ~/.config/opencode/plugins/ox-todo
```

Requires the ox server (this repository) to be running: the plugin reads
`state.metadata.phases` off `todo` tool results, which only ox's tool emits.

## How it works

- Data source: the latest **completed** `todo` tool call before the session's
  revert boundary — the same selection rule as opencode's own `sessionTodos`.
- `state.metadata.phases` shape (from ox `src/tool/todo.rs::encode_phases`):

  ```json
  [
    { "name": "Implement",
      "tasks": [
        { "content": "write module", "status": "in_progress" },
        { "content": "wait for CI", "status": "blocked", "blocker": "flaky runner" }
      ] }
  ]
  ```

- Statuses are ox's full set: `pending`, `in_progress`, `completed`,
  `abandoned`, `blocked`. Unknown statuses drop the task rather than showing
  it as pending.
- Mounted with `append: "sidebar.content"` so the built-in context/MCP/todo
  blocks survive; the built-in `Todo` block stays empty for ox sessions
  (it only reads `todowrite`).
- Collapse state persists via `context.storage.store("view")`, same as the
  built-in sidebar blocks.

## Files

| File | Purpose |
| --- | --- |
| `tui.tsx` | Plugin definition + Solid components (header, phases, tasks) |
| `parse.ts` | `sessionPhases` (selection), `parsePhases` (decoding), counters |
| `parse.test.ts` | 7 cases: decoding tolerance, selection, revert boundary (`bun test`) |

## Notes

- The plugin does **not** import `@opencode/client`: messages are validated
  structurally (`unknown` in, records checked field by field), so a host type
  change cannot break the plugin's build.
- `abandoned` tasks render with the faint colour but no strikethrough —
  opentui's attributes apply to the whole `<text>` row and would strike the
  marker too.
- Message-stream rendering is unchanged: ox's `todo` still falls through to the
  generic tool card there (its collapsed summary is built from `input`). Only
  the sidebar reads `metadata`.
