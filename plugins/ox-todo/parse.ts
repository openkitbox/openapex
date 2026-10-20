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
 * The list in force for a session: the metadata of the latest **completed**
 * `todo` call before the revert boundary.
 *
 * Same selection rule as opencode's own `sessionTodos`
 * (`packages/tui/src/util/todo.ts`): revert hides everything from the boundary
 * on, and only a completed call has a settled list to show.
 */
export function sessionPhases(messages: ReadonlyArray<unknown>, boundary?: string): TodoPhase[] {
  const boundaryIndex = boundary ? messages.findIndex((message) => idOf(message) === boundary) : -1
  const scoped = messages.slice(0, boundaryIndex === -1 ? messages.length : boundaryIndex)
  for (let index = scoped.length - 1; index >= 0; index--) {
    const message = scoped[index]
    if (!isRecord(message) || message.type !== "assistant" || !Array.isArray(message.content)) continue
    for (let part = message.content.length - 1; part >= 0; part--) {
      const item = message.content[part]
      if (!isRecord(item) || item.type !== "tool" || item.name !== TOOL) continue
      const state = item.state
      if (!isRecord(state) || state.status !== "completed") continue
      // The newest completed call wins even when it carries no metadata: it is
      // still the state the session is in, so an older list must not resurface.
      return parsePhases(isRecord(state.metadata) ? state.metadata.phases : undefined)
    }
  }
  return []
}

function idOf(message: unknown): string | undefined {
  return isRecord(message) && typeof message.id === "string" ? message.id : undefined
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
