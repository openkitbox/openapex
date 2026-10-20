/**
 * Decoding for ox's two-level todo model, as published in a `todo` tool
 * result's `state.metadata.phases` (see `.opencode/plans/ox-todo-metadata.md`
 * and `src/tool/todo.rs`'s `encode_phases`).
 *
 * Everything here is structural and runtime-checked: no `@opencode/client`
 * type is imported, so the plugin keeps working (and keeps type-checking)
 * when the host's generated types move. Malformed rows are skipped rather than
 * thrown on — the same tolerance ox applies when re-reading its own durable
 * row (`decode_phases`), so a partially bad payload degrades instead of
 * blanking the panel.
 */

/** ox's full status set, including the two opencode's `todowrite` lacks. */
export type TodoStatus = "pending" | "in_progress" | "completed" | "abandoned" | "blocked"

export type TodoTask = {
  readonly content: string
  readonly status: TodoStatus
  /** Free-text note a `block` op attached; ox collapses it to one line. */
  readonly blocker?: string
}

export type TodoPhase = {
  readonly name: string
  readonly tasks: readonly TodoTask[]
}

/** The tool whose metadata this plugin renders. */
const TOOL = "todo"

const statuses = new Set<unknown>(["pending", "in_progress", "completed", "abandoned", "blocked"])

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

export function parseTask(value: unknown): TodoTask | undefined {
  if (!isRecord(value)) return
  const { content, status } = value
  if (typeof content !== "string" || !content) return
  if (!statuses.has(status)) return
  const blocker = typeof value.blocker === "string" && value.blocker ? value.blocker : undefined
  return blocker ? { content, status: status as TodoStatus, blocker } : { content, status: status as TodoStatus }
}

/** Decode `metadata.phases`; unknown statuses drop the task, not the tree. */
export function parsePhases(value: unknown): TodoPhase[] {
  if (!Array.isArray(value)) return []
  return value.flatMap((phase) => {
    if (!isRecord(phase) || typeof phase.name !== "string" || !Array.isArray(phase.tasks)) return []
    return [{ name: phase.name, tasks: phase.tasks.flatMap((task) => parseTask(task) ?? []) }]
  })
}

/**
 * Find the todo list in force inside a **newest-first** page.
 *
 * Same selection rule as opencode's own `sessionTodos`
 * (`packages/tui/src/util/todo.ts`): revert hides everything from the boundary
 * on, and only a completed call has a settled list to show. The newest
 * completed call wins even when it carries no metadata — that is still the
 * state the session is in, so an older list must not resurface.
 *
 * The two "nothing to show" cases are distinguished so a caller can page:
 * - `undefined` — this page holds no completed call, so older history may.
 * - `[]` — the newest completed call exists but has nothing to show: either it
 *   was recorded by an ox build from before the metadata channel, or it is the
 *   call that *finished* the list and ox cleared it (`metadata.cleared`). Both
 *   stop the walk; there is nothing older that is still in force.
 */
export function phasesFromDesc(messages: readonly unknown[], boundary?: string): TodoPhase[] | undefined {
  // Messages at and after the revert boundary are hidden, so skip them while
  // the walk is still inside that region.
  let skipping = boundary !== undefined
  for (const message of messages) {
    if (!isRecord(message)) continue
    if (skipping) {
      if (message.id === boundary) skipping = false
      continue
    }
    if (message.type !== "assistant" || !Array.isArray(message.content)) continue
    // Parts are in generation order; the last todo in a message is the latest.
    for (let index = message.content.length - 1; index >= 0; index--) {
      const part = message.content[index]
      if (!isRecord(part) || part.type !== "tool" || part.name !== TOOL) continue
      const state = part.state
      if (!isRecord(state) || state.status !== "completed") continue
      const metadata = isRecord(state.metadata) ? state.metadata : undefined
      // ox drops the stored list when its last task closes. The result still
      // carries the finished tree (so the transcript shows the confirmation),
      // but the session's list is empty from then on — a client panel must
      // clear with it.
      if (metadata?.cleared === true) return []
      return parsePhases(metadata?.phases)
    }
  }
  return undefined
}

/**
 * [`phasesFromDesc`] for an ascending list (what `data.session.message.list`
 * returns): the newest completed call before the revert boundary, or `[]` when
 * the window holds none.
 */
export function sessionPhases(messages: ReadonlyArray<unknown>, boundary?: string): TodoPhase[] {
  return phasesFromDesc([...messages].reverse(), boundary) ?? []
}

/** Ox's `is_open`: what counts as remaining. */
function isOpen(task: TodoTask) {
  return task.status === "pending" || task.status === "in_progress"
}

export function taskCount(phases: readonly TodoPhase[]) {
  return phases.reduce((total, phase) => total + phase.tasks.length, 0)
}

export function remainingCount(phases: readonly TodoPhase[]) {
  return phases.reduce((total, phase) => total + phase.tasks.filter(isOpen).length, 0)
}

export function blockedCount(phases: readonly TodoPhase[]) {
  return phases.reduce(
    (total, phase) => total + phase.tasks.filter((task) => task.status === "blocked").length,
    0,
  )
}

/** Completed or abandoned: no longer work. */
export function closedCount(phases: readonly TodoPhase[]) {
  return taskCount(phases) - remainingCount(phases) - blockedCount(phases)
}

export function phaseCounts(phase: TodoPhase) {
  const done = phase.tasks.filter((task) => task.status === "completed" || task.status === "abandoned").length
  return { done, total: phase.tasks.length }
}
