import { Plugin } from "@opencode/plugin/tui"
import { For, Show, createMemo } from "solid-js"
import {
  blockedCount,
  phaseCounts,
  remainingCount,
  sessionPhases,
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

export function OxTodo(props: { context: Plugin.Context; sessionID: string }) {
  const [view, updateView] = props.context.storage.store("view", { initial: { open: true } })
  const theme = props.context.theme
  const phases = createMemo(() =>
    sessionPhases(
      props.context.data.session.message.list(props.sessionID),
      props.context.data.session.get(props.sessionID)?.revert?.messageID,
    ),
  )
  const total = createMemo(() => taskCount(phases()))
  const remaining = createMemo(() => remainingCount(phases()))
  const blocked = createMemo(() => blockedCount(phases()))

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
              {/* Collapsed: the number that matters, matching the MCP block's shape. */}
              <span style={{ fg: theme.text.muted }}>
                {" "}
                ({remaining()} remaining{blocked() > 0 ? `, ${blocked()} blocked` : ""})
              </span>
            </Show>
          </text>
        </box>
        <Show when={total() <= 2 || view.open}>
          <For each={phases()}>{(phase) => <Phase theme={theme} phase={phase} />}</For>
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
