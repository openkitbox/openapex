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
| Frozen head | `m[0]` = `<session-history>` (compartment set), `m[1]` = pinboard + new compartments + the anchoring placeholder. The head replaces the transcript range it covers; the live tail follows. |
| Historian | At a step boundary, when the live tail passes a pressure band, ox folds the oldest part of the tail into compartments with a hidden completion through cardinal. |
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
# Pressure bands over the 60k-token history budget: at `soft` a run may start
# pre-emptively, at `hard` it must. Violated pairs fall back to these with a
# warning.
soft_threshold = 0.70
hard_threshold = 0.85
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

## Operator signals

- `historian pressure tail_tokens=… trigger=Forced|Speculative|No` (debug) — the
  measurement behind each firing decision.
- `historian chunk chunk_start=… chunk_end=… chunk_tokens=…` (debug) — what the
  run will cover. The chunk budget (75% of the history budget) is also the
  fence: the newest part of the tail always stays verbatim.
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
- **Hidden completions are real completions.** A historian run summarizes up to
  75% of the history budget; the dreamer only fires when a project holds
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
  start with `src/mctx/dreamer.rs`, `src/mctx/search.rs` and
  `src/runner/historian_trigger.rs`.
