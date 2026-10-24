# magic-context in ox

ox carries a native implementation of magic-context's memory layer. It is on by
default; `[magic_context] enabled = false` turns the whole thing back into a
plain chat server (no pinboard, no `§N§` overlay, no frozen head, no historian,
no dreamer).

## What it does

| Layer | Effect |
|---|---|
| `ctx_memory` / `ctx_note` | Durable facts (`#N` handles) and pending notes, stored in ox's own `mc_*` tables. |
| Pinboard | The project's active memories, rendered into the request as `<project-memory>` inside **m[1]** (the volatile half), so a memory write never moves the provider's cache anchor. |
| `§N§` overlay | Every transcript item is tagged in the request; the model can address items with `ctx_reduce` (drop) and recall them with `ctx_expand`. |
| Frozen head | `m[0]` = `<session-history>` (compartment set, decay-rendered at the session's history budget), `m[1]` = pinboard + the anchoring placeholder. The head replaces the transcript range it covers; the live tail follows. A fold rebuilds m[0] — the head is always a function of the current compartment set, never a frozen baseline plus a delta. |
| Historian | At a step boundary, when the live tail passes a pressure band (or exceeds its trigger budget) and still has content outside the protected tail, ox folds the oldest part of the tail into compartments with a hidden completion through cardinal. |
| Drops | A `ctx_reduce` stamp is applied by the next head rebuild *and* by the read path: the session API renders an applied drop's content as `[dropped §N§]`, the same placeholder the model sees. |
| Dreamer | Every `dreamer_interval_minutes`, per project with memories: `verify` (do cited files still back the claims?) and `curate` (merge duplicates, archive redundancy). |
| `ctx_search` | Memories, notes, compartments (turso FTS indexes) and a transcript scan. With `[magic_context.embedding]` configured, the **memory lane** ranks by cosine similarity instead. |

## Configuration

```toml
[magic_context]
# Master switch (default true).
enabled = true
# Model for hidden completions (historian + dreamer), as provider/id.
# Unset = ox's default model.
historian_model = "cardinal/k3-256k"
# Context usage (percent of the model window) at which folding is due. Every
# other number is derived from it: the proactive floor is `execute - 2`, the
# force band is `max(85, execute + 2)`, the trigger budget is 5% of the usable
# window (floor 5k tokens), the historian's chunk budget is 25% of ITS OWN
# window, and the protected tail is 30% of the usable window. Range 20-90.
execute_threshold_percentage = 65.0
# Share of the usable window the m[0] history block may carry before the decay
# ladder demotes older compartments. Range 0.05-0.5.
history_budget_percentage = 0.15
# Scheduled memory maintenance (default true) and its cadence.
dreamer_enabled = true
dreamer_interval_minutes = 15

# Semantic search for project memories. Omit the block (or provider = "off")
# and the memory lane stays lexical.
[magic_context.embedding]
provider = "openai-compatible"        # or "off" (default)
endpoint = "https://open.bigmodel.cn/api/paas/v4"   # base URL, no /embeddings
model = "embedding-3"
api_key = "…"
# query_instruction = "query: "       # for instruction-tuned models
# document_prefix = "search_document: "  # part of the vector identity
```

Vectors live beside the text (`mc_memory.embedding`, `embedding_model`) and are
written on the memory write itself; a failed embedding call never fails the
write — the row simply has no vector and the next `ctx_search` backfills up to
20 of them. Changing the model or the document prefix changes the identity, so
every row is re-embedded by the same backfill instead of being compared across
vector spaces. A search whose embedding call fails degrades to the index with a
warning.

Comparison is an in-memory cosine over the stored `f32` bytes (`crates/ox-engine/src/mctx/embed.rs`);
ox does not use turso's built-in vector index — only the FTS indexes are turso's.

Verified end to end against a running instance (2026-10-19): a memory written with
`ctx_memory`, then queried as *"which storage engine keeps the durable rows and
the inverted word index"* — a query sharing **no literal word** with the stored
text (`turso`, `ox.db`, `Tantivy`, `fts` all absent) — returned that memory as
`[memory] id=#4`. That is semantic retrieval, not a keyword hit.

## What the client shows

- **Context percentage.** The TUI divides the last assistant turn's tokens
  (input + output + reasoning + cache read/write) by the model's
  `limit.context` (`packages/tui/src/util/session.ts:46-63`). ox serves that
  number from `[[models]]`: add `context = 262144` (and optionally
  `output = 32768`). Without it the client shows the token count and no
  percentage — never a guessed one.
- **Window caps.** ox.toml `[context_caps]` lowers a model's window for the
  compaction geometry: `min(reported, cap)` — e.g. treat an advertised-1M
  model as 256k by adding `"model-id" = 262144`. Caps never raise a window;
  a cap on a model the gateway reports no context for is inert, and inert or
  unknown caps warn at each catalogue refresh.
- **Memory-operation notice lines.** When a historian run publishes
  compartments, ox appends a `synthetic` message with an empty `text` and a
  `description` such as `historian folded 99..155 · 1 compartment`. The TUI
  renders that as an inline `◈` notice row in the transcript, and because the
  text is empty the model never pays for it. Memory *tool* calls
  (`ctx_memory`, `ctx_reduce`, …) already appear as their own tool rows, so
  they need no notice.

## Operator signals

- `historian pressure tail_tokens=… usage_pct=… trigger_budget=… trigger=…`
  (debug) — the measurement behind each firing decision. `usage_pct` is empty
  until the session's first completion reports token counts.
- `historian chunk chunk_start=… chunk_end=… chunk_tokens=…` (debug) — what the
  run will cover. The chunk budget is 25% of the *historian's own* window,
  further capped so the protected tail (30% of this session's usable window)
  stays verbatim.
- `historian published run=hrun_… covered=… compartments=N` — one folded range.
- `dreamer task is due` / `dreamer task skipped detail=…` (debug) — why a
  maintenance task did or did not spend a completion.
- Tables: `mc_memory`, `mc_note`, `mc_compartments`, `mc_drop`, `mc_tag`,
  `mc_frozen_state`, `mc_historian_run`, `mc_dream_state`.

## Constraints worth knowing

- **`ox.db` belongs to ox.** turso writes FTS indexes with `CREATE INDEX … USING
  fts`, which stock SQLite (and therefore upstream opencode) cannot parse, and
  it takes an exclusive lock on the file. External tools cannot read the
  database while ox runs; inspect through ox's API or a turso probe instead.
  The schema incompatibility is not only a locking matter: even with the lock
  bypassed (`mode=ro&immutable=1`), SQLite fails on the first schema read with
  `malformed database schema (__turso_internal_fts_dir_mc_memory_fts_key) - near
  "USING": syntax error`. A pre-migration copy lives at `~/.ox/ox.db.pre-turso`,
  but it stops at the moment of the switch.
- **Hidden completions are real completions.** A historian run summarizes up to
  25% of its own model's window; the dreamer only fires when a project holds
  memories whose cited files changed (or two memories share a category).
- **The dreamer's verify gate is confined to the project**: a memory may only
  cite files inside the session's directory (no `~`, no `..` escape, no symlink
  out), and a cited file larger than a few kilobytes is read only up to the
  excerpt budget.
- **Embeddings are a second outbound path.** Chat traffic goes through
  cardinal; embeddings do not (cardinal has no `/embeddings` route). The key
  lives in `ox.toml` beside the cardinal key.
- Design deviations from the reference (budget-as-fence, no `map-memories`, no
  staleness signal, one in-process claim instead of leases, semantic ranking
  only for memories) are documented at the top of the module that owns them —
  start with `crates/ox-engine/src/mctx/dreamer.rs`, `crates/ox-engine/src/mctx/search.rs` and
  `crates/ox-engine/src/runner/historian_trigger.rs`.
