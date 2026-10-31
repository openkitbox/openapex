---
name: opencode-parity
description: 对比上游 opencode（TUI 与 server API）和 ox 的差异并推动对齐。用于：上游发新版本后评估要跟进什么；对齐某个具体功能或组件（如 Explored、prompt、对话框）；检查 ox 是否漏了上游的 keybind、斜杠命令、对话框、SDK 调用、SSE 事件或 API endpoint。
---

# opencode-parity

把上游 opencode 当作行为规范，找出 ox 与它的差异。对齐的是**用户可见的行为**，不是上游的代码结构。

- 上游源码：`/Users/john/github/opencode`（也可经 `.ox/reference/agents/opencode` 访问）。TUI 在 `packages/tui/src`，协议在 `packages/protocol/openapi.json`。
- ox TUI：`crates/ox-tui/src`。ox server：`crates/ox-server/src`（路由 `api.rs` 的 `build_router`）。
- 硬约束：ox-tui 必须兼容上游 OpenCode TUI 2.2（项目记忆 #296）。

## 选场景

| 要做的事 | 走哪条 |
|---|---|
| 上游升版（如 v2.2.3 → v2.3.0），问"要跟进什么" | A：升版 |
| 对齐一个具体功能或组件 | B：单功能 |
| 像素/单元格级的外观对齐 | C：视觉，直接用 `crates/ox-tui/tests/parity/`（见其 README），不要在这里重做 |

## 场景 A：升版

在本目录下执行（先 `unset MallocStackLogging MallocStackLoggingNoCompact`，否则子进程会往 stderr 写 malloc 警告）：

1. `python3 openapi_diff.py OLD_TAG NEW_TAG`：协议层有哪些 path、operation、参数、schema 变化。
2. `python3 features.py delta OLD_TAG NEW_TAG`：TUI 功能 ID（keybind、斜杠命令、对话框、SDK 调用、SSE 事件、主题 token）新增和删除了哪些，每个变更 ID 在台账里的现状。
3. 对每个新增 ID，按场景 B 评估；对每个删除 ID，把台账行改状态。
4. 完成后更新 `features.md` 表头里的基线 tag。

## 场景 B：单功能

1. `rg` 台账 `features.md` 找该功能的行，读上游 `file:line` 和 ox `file:line` 以及状态。
2. 读上游源码，按 `component-checklist.md` 逐项核对：状态、输入、字段与主题 token、边界、副作用。
3. 读 ox 对应代码，对每一项给出结论。
4. 用固定格式输出缺口表：`缺口 | 上游 file:line | ox file:line | 用户可见 | 体量 | 决策`。
5. 实施对齐走项目的开发流程（计划文档 + worktree）。完成后回填台账行的状态、ox `file:line`，有意偏差写成 `deviation` 并写明原因。

## 台账命令

- `python3 features.py check`：上游提取出的每个 ID 都必须出现在 `features.md`，缺项报错。台账修改后和升版后都要跑。
- `python3 features.py seed`：为台账里还没有的 ID 追加 `todo` 行，不改已有行。
- 状态取值：`aligned`、`partial`、`missing`、`deviation`（必须写原因）、`n/a`（必须写原因）、`todo`。

## 硬规则

1. 两侧结论都带 `file:line`，不凭印象。读不到的就写"未核实"。
2. 对齐行为，不对齐架构。上游是 TSX + opentui，ox 是 ratatui，不做逐行翻译。
3. 每个有意偏差都写进 `features.md` 对应行（`deviation` + 原因），不另建台账。历史偏差散在 `.ox/plans/*.md`，用 `rg -n "偏差|deviation" .ox/plans` 查。
4. 提取脚本依赖上游的书写习惯。`features.py check` 报某类数量骤降时，先怀疑提取器失效而不是上游删了功能。
5. 自动提取的 kind：`keybind`、`slash`、`slash_alias`、`dialog`、`sdk_call`、`sse_event`、`theme_token`、`theme_token_v2`、`tool_renderer`。消息 part 类型（来自 `@opencode/client`，不在 TUI 目录）和 CLI/配置项（在 `packages/cli`）不可靠提取，需手工登记。
6. 手工登记的行用自己的 kind，例如 `behavior`（整体行为：Explored 展开、悬停高亮、拖选复制、粘贴折叠、粘贴附件）、`part_type`、`cli_option`。`check` 不会把这些 kind 报为"上游已删除"。
7. 提取结果的盲区：插件上下文里的 `props.context.client.<ns>.*` 调用，以及 `mini/` 的 `sdk.*` 调用不会被提取；`sse_event` 里 `mini/` 的内部发射器名（`turn.*`、`stream.*`、`queued.prompts`）不是 SSE 事件，已标 `n/a`。
8. 服务端只比较 API endpoint。

## 当前台账状态（2026-10-31，基线 v2.2.3）

`features.md` 有 556 行自动登记的 `todo`，其中已核对的是：斜杠命令里 ox 已有的 10 个同名命令和 4 个别名（只核对了名字，行为未逐项核对）、4 个对话框、`prompt.paste`，以及 5 条手工 `behavior` 示范行（含偏差原因）。其余 `todo` 行代表**尚未评估**，不代表缺失。`n/a` 标注中带"待用户确认"的需用户确认。

ox 目前只注册了 10 个斜杠命令，上游有 38 个，这是最明显的差距来源。

## 文件

- `extract_tui.py`：从上游源码（可指定 git ref）提取功能 ID。
- `features.py`：台账的 seed、check、delta。
- `openapi_diff.py`：两个上游 tag 的 `openapi.json` 差异。
- `features.md`：功能台账，唯一的持久状态。
- `component-checklist.md`：场景 B 的逐项清单。
- `tests/`：脚本的单元测试，`python3 -m unittest discover -s tests`。
