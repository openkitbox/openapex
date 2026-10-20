import { Plugin } from "@opencode/plugin/tui"
import { For, Show, createMemo, createResource, createSignal, onCleanup } from "solid-js"
import {
  blockedCount,
  phaseCounts,
  phasesFromDesc,
  remainingCount,
  taskCount,
  type TodoPhase,
  type TodoStatus,
  type TodoTask,
} from "./parse"

/**
 * ox's two-level todo list in opencode's sidebar.
 *
 * Reads the structured `phases` ox's `todo` tool publishes in
 * `ToolOutput.metadata` (`{ "phases": [{ name, tasks: [{ content, status,
 * blocker? }] }] }`) — nothing is parsed out of the rendered text. Install this
 * directory as a TUI plugin (see README.md); it appends a `Todo` block to
 * `sidebar.content`.
 *
 * `append`, not `replace`: `sidebar.content` also carries the built-in
 * context/MCP/todo blocks, and replacing the slot would suppress all of them.
 * ox sessions keep that built-in todo block empty (it only reads `todowrite`),
 * so the two never compete.
 */

/** Page size for the history walk. */
const PAGE = 100
/** Hard stop: ~2k messages is far more history than finding a list ever needs. */
const MAX_PAGES = 20

const marker = (status: TodoStatus) => {
  if (status === "completed") return "[✓]"
  if (status === "in_progress") return "[•]"
  if (status === "abandoned") return "[-]"
  if (status === "blocked") return "[!]"
  return "[ ]"
}

function Task(props: { theme: Plugin.Context["theme"]; task: TodoTask }) {
  const fg = () => {
    if (props.task.status === "in_progress") return props.theme.text.feedback.warning.base
    if (props.task.status === "blocked") return props.theme.text.feedback.error.base
    if (props.task.status === "abandoned") return props.theme.text.faint
    return props.theme.text.muted
  }

  return (
    <box flexDirection="row" gap={0} paddingLeft={2}>
      <text flexShrink={0} fg={fg()}>
        {marker(props.task.status)}{" "}
      </text>
      <text flexGrow={1} wrapMode="word" fg={fg()}>
        {props.task.content}
        <Show when={props.task.blocker}>
          <span style={{ fg: props.theme.text.faint }}> — {props.task.blocker}</span>
        </Show>
      </text>
    </box>
  )
}

function Phase(props: { theme: Plugin.Context["theme"]; phase: TodoPhase }) {
  const counts = createMemo(() => phaseCounts(props.phase))

  return (
    <box>
      <text>
        <span style={{ fg: props.theme.text.base }}>{props.phase.name}</span>
        <span style={{ fg: props.theme.text.muted }}>
          {" "}
          ({counts().done}/{counts().total})
        </span>
      </text>
      <For each={props.phase.tasks}>{(task) => <Task theme={props.theme} task={task} />}</For>
    </box>
  )
}

/**
 * The list in force for a session, walking history backwards until a completed
 * `todo` call turns up.
 *
 * The host transcript holds only the initial page (20 messages) plus whatever
 * has scrolled past, so a session whose last `todo` call is older than that
 * window would otherwise render empty. Paging here keeps the panel right for
 * long sessions without asking the host for a wider window.
 */
async function loadPhases(
  context: Plugin.Context,
  sessionID: string,
  boundary: string | undefined,
): Promise<TodoPhase[]> {
  // The loaded window already holds the newest messages: the common case costs
  // no request at all.
  const loaded = phasesFromDesc([...context.data.session.message.list(sessionID)].reverse(), boundary)
  if (loaded) return loaded

  // Otherwise page backwards. Each page carries its own cursor, so the walk does
  // not depend on how much the host happens to have loaded.
  let cursor: string | undefined
  for (let page = 0; page < MAX_PAGES; page++) {
    const response = await context.client.message.list({ sessionID, limit: PAGE, order: "desc", cursor })
    const found = phasesFromDesc(response.data, boundary)
    if (found) return found
    // A short page means history is exhausted; a full one carries the cursor.
    if (response.data.length < PAGE) break
    cursor = response.cursor.next ?? undefined
    if (!cursor) break
  }
  return []
}

export function OxTodo(props: { context: Plugin.Context; sessionID: string }) {
  const [view, updateView] = props.context.storage.store("view", { initial: { open: true } })
  const theme = props.context.theme
  const boundary = createMemo(() => props.context.data.session.get(props.sessionID)?.revert?.messageID)
  // Bumped when this session settles a tool call or changes its revert state —
  // the only things that can change the list.
  const [revision, setRevision] = createSignal(0)
  const stops = [
    props.context.data.on("session.tool.success", (event) => {
      if (event.data.sessionID === props.sessionID) setRevision((value) => value + 1)
    }),
    props.context.data.on("session.revert.committed", () => setRevision((value) => value + 1)),
    props.context.data.on("session.revert.cleared", () => setRevision((value) => value + 1)),
  ]
  onCleanup(() => stops.forEach((stop) => stop()))

  const [phases] = createResource(
    () => [props.sessionID, boundary(), revision()] as const,
    ([sessionID, revert]) => loadPhases(props.context, sessionID, revert),
  )
  const list = createMemo(() => phases() ?? [])
  const total = createMemo(() => taskCount(list()))
  const remaining = createMemo(() => remainingCount(list()))
  const blocked = createMemo(() => blockedCount(list()))

  return (
    <Show when={total() > 0}>
      <box>
        <box
          flexDirection="row"
          gap={1}
          onMouseDown={() => {
            if (total() <= 2) return
            void updateView((draft) => {
              draft.open = !draft.open
            }).catch((error) => console.error("Failed to persist ox-todo sidebar state", error))
          }}
        >
          <Show when={total() > 2}>
            <text fg={theme.text.base}>{view.open ? "▼" : "▶"}</text>
          </Show>
          <text fg={theme.text.base}>
            <b>Todo</b>
            <Show when={!view.open}>
              <span style={{ fg: theme.text.muted }}>
                {" "}
                ({remaining()} remaining{blocked() > 0 ? `, ${blocked()} blocked` : ""})
              </span>
            </Show>
          </text>
        </box>
        <Show when={total() <= 2 || view.open}>
          <For each={list()}>{(phase) => <Phase theme={theme} phase={phase} />}</For>
        </Show>
      </box>
    </Show>
  )
}

export default Plugin.define({
  id: "ox.sidebar.todo",
  setup(context) {
    context.ui.slot({
      append: "sidebar.content",
      render: (props) => <OxTodo context={context} sessionID={props.sessionID} />,
    })
  },
})
