import { expect, test } from "bun:test"
import {
  blockedCount,
  closedCount,
  parsePhases,
  phaseCounts,
  phasesFromDesc,
  remainingCount,
  sessionPhases,
  taskCount,
} from "./parse"

test("parses ox phases including blocked and abandoned", () => {
  const phases = parsePhases([
    {
      name: "Investigate",
      tasks: [
        { content: "read", status: "in_progress" },
        { content: "grep", status: "pending" },
        { content: "wait", status: "blocked", blocker: "flaky CI" },
        { content: "skip", status: "abandoned" },
        { content: "done", status: "completed" },
      ],
    },
  ])
  expect(phases).toEqual([
    {
      name: "Investigate",
      tasks: [
        { content: "read", status: "in_progress" },
        { content: "grep", status: "pending" },
        { content: "wait", status: "blocked", blocker: "flaky CI" },
        { content: "skip", status: "abandoned" },
        { content: "done", status: "completed" },
      ],
    },
  ])
  expect(taskCount(phases)).toBe(5)
  expect(remainingCount(phases)).toBe(2)
  expect(blockedCount(phases)).toBe(1)
  expect(closedCount(phases)).toBe(2)
  expect(phaseCounts(phases[0]!)).toEqual({ done: 2, total: 5 })
})

// The coercion upstream's `parseTodos` does (`blocked` → `pending`) is exactly
// what this plugin avoids: it would turn "waiting on CI" into invisible work.
test("drops unknown statuses and malformed rows instead of coercing them", () => {
  const phases = parsePhases([
    {
      name: "P",
      tasks: [
        { content: "a", status: "pending" },
        { content: "b", status: "cancelled" },
        { content: "", status: "pending" },
        { status: "pending" },
        { content: "c", status: "pending", blocker: "" },
        null,
      ],
    },
    null,
    { tasks: [] },
    { name: "Q", tasks: "nope" },
  ])
  expect(phases).toEqual([
    { name: "P", tasks: [{ content: "a", status: "pending" }, { content: "c", status: "pending" }] },
  ])
})

test("non-array payloads are empty, not an error", () => {
  expect(parsePhases(undefined)).toEqual([])
  expect(parsePhases(null)).toEqual([])
  expect(parsePhases({})).toEqual([])
  expect(parsePhases("x")).toEqual([])
})

const tool = (id: string, status: string, phases?: unknown) => ({
  id,
  type: "assistant",
  content: [
    {
      type: "tool",
      id: `tool_${id}`,
      name: "todo",
      state: {
        status,
        input: {},
        content: [{ type: "text", text: "" }],
        metadata: phases === undefined ? undefined : { phases },
      },
    },
  ],
})

test("uses the latest completed todo, honouring the revert boundary", () => {
  const messages = [
    tool("m1", "completed", [{ name: "A", tasks: [{ content: "a", status: "pending" }] }]),
    tool("m2", "completed", [{ name: "B", tasks: [{ content: "b", status: "pending" }] }]),
    tool("m3", "streaming", [{ name: "C", tasks: [{ content: "c", status: "pending" }] }]),
  ]
  expect(sessionPhases(messages)).toEqual([{ name: "B", tasks: [{ content: "b", status: "pending" }] }])
  // Reverting to m2 hides m2 onwards.
  expect(sessionPhases(messages, "m2")).toEqual([{ name: "A", tasks: [{ content: "a", status: "pending" }] }])
  expect(sessionPhases(messages, "m1")).toEqual([])
})

// The paging contract: `undefined` means "this page has no completed call, ask
// for an older one"; `[]` means "found it, but there is nothing to show".
test("phasesFromDesc distinguishes missing from empty", () => {
  const withMetadata = [tool("m2", "completed", [{ name: "B", tasks: [{ content: "b", status: "pending" }] }])]
  const withoutMetadata = [tool("m2", "completed")]
  const noCalls = [{ id: "u1", type: "user", content: [] }]

  expect(phasesFromDesc(withMetadata)).toHaveLength(1)
  expect(phasesFromDesc(withoutMetadata)).toEqual([])
  expect(phasesFromDesc(noCalls)).toBeUndefined()
})

// ox clears a finished list. The result still carries the completed tree, and a
// panel must not resurrect it from an older call.
test("a cleared result ends the walk with an empty list", () => {
  const cleared = [
    {
      id: "m3",
      type: "assistant",
      content: [
        {
          type: "tool",
          id: "t3",
          name: "todo",
          state: {
            status: "completed",
            input: {},
            metadata: {
              cleared: true,
              phases: [{ name: "P", tasks: [{ content: "done", status: "completed" }] }],
            },
          },
        },
      ],
    },
  ]
  expect(phasesFromDesc(cleared)).toEqual([])
  // Even with an older, still-open list behind it.
  const older = tool("m1", "completed", [{ name: "A", tasks: [{ content: "a", status: "pending" }] }])
  expect(phasesFromDesc([...cleared, older])).toEqual([])
})

test("the newest completed call wins even without metadata", () => {
  const messages = [
    tool("m1", "completed", [{ name: "A", tasks: [{ content: "a", status: "pending" }] }]),
    tool("m2", "completed"),
  ]
  expect(sessionPhases(messages)).toEqual([])
})

test("ignores other tools and unfinished calls", () => {
  const messages = [
    { id: "u1", type: "user", content: [] },
    {
      id: "a1",
      type: "assistant",
      content: [
        {
          type: "tool",
          id: "t1",
          name: "todowrite",
          state: { status: "completed", input: {}, metadata: { phases: [{ name: "no", tasks: [] }] } },
        },
      ],
    },
    {
      id: "a2",
      type: "assistant",
      content: [
        {
          type: "tool",
          id: "t2",
          name: "todo",
          state: { status: "error", input: {}, metadata: { phases: [{ name: "no", tasks: [] }] } },
        },
      ],
    },
  ]
  expect(sessionPhases(messages)).toEqual([])
})

test("multiple todo calls inside one assistant message take the last", () => {
  const message = {
    id: "a1",
    type: "assistant",
    content: [
      {
        type: "tool",
        id: "t1",
        name: "todo",
        state: { status: "completed", input: {}, metadata: { phases: [{ name: "first", tasks: [{ content: "x", status: "pending" }] }] } },
      },
      {
        type: "tool",
        id: "t2",
        name: "todo",
        state: { status: "completed", input: {}, metadata: { phases: [{ name: "second", tasks: [{ content: "y", status: "pending" }] }] } },
      },
    ],
  }
  expect(sessionPhases([message])).toEqual([{ name: "second", tasks: [{ content: "y", status: "pending" }] }])
})
