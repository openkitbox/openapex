# Session environment API 对齐方案

## 分析与方案

### 现象与根因

用户请求：

```text
NotFoundError: No route for PUT /api/session/ses_1cf9d5a77001PAxQ03HauaeIsh/environment
```

已核实 ox 当前路由在 `crates/ox-server/src/api.rs:1011-1067` 挂载了 `GET/PATCH/DELETE /api/session/{sessionID}` 及多个子路由，但没有 `PUT /api/session/{sessionID}/environment`，因此请求落入 `unmatched`（同文件 `:1070-1083`）并返回 404。该现象不是 session ID 不存在，而是路由缺失。

上游 `/Users/john/github/opencode` 的行为已核实：

- `packages/protocol/openapi.json:4625-4697` 定义 `PUT /api/session/{sessionID}/environment`，请求体必须是 `{ "variables": Record<string,string> }`，成功返回 204，未知 session 返回 `SessionNotFoundError`。
- `packages/server/src/handlers/session.ts:192-200` 调用 `session.environment({ sessionID, variables })` 后返回 204。
- `packages/core/src/session.ts:395-400` 先校验 session，再在变量字段存在时替换环境并返回当前值；HTTP PUT 始终携带变量，因此这里是替换而非增量合并。
- `packages/core/src/session/environment.ts:7-29` 以进程内 `Map<sessionID, Variables>` 保存环境，`set` 复制变量对象，`clear` 在删除 session 时移除它。
- `packages/core/src/shell.ts:880-899` 对本地 shell 使用 session 环境覆盖当前进程环境，并额外注入 `TERM=xterm-256color` 与 `OPENCODE_TERMINAL=1`；workspace session 有特殊分支，但 ox 当前没有对应 workspace shell 路径，本次不扩展该范围。
ox 当前 shell 边界已核实：

- `crates/ox-tools/src/shell/sessions.rs:864-910` 的 `ShellSessions` 是模型 shell 与 HTTP `session.shell` 共用的进程级 shell 域。
- `crates/ox-tools/src/shell/sessions.rs:845-862` 的 `SpawnRequest` 已携带 `Owner.session_id`，适合在 spawn 时按 session 读取环境，不需要让 API 直接操作进程层。
- `crates/ox-tools/src/shell/process.rs:106-137` 是唯一 OS spawn 边界，当前只继承父进程环境并移除 `HERDR_*`；在此边界应用 session 覆盖环境可同时覆盖模型 shell 与 `session.shell`。
- `crates/ox-server/src/api/session/user_shell.rs:58-82` 已通过 `Owner.session_id` 启动用户 shell；`crates/ox-tools/src/shell/mod.rs:416-427` 已为模型 shell 设置相同 owner。
- `crates/ox-server/src/api/session/lifecycle.rs:340-400` / `delete_session_tree` 是 session 删除路径，必须清除对应环境，避免长期运行进程保留已删除 session 的状态。

### 设计决策

1. 新增 session API handler，职责仅为：解析 `{variables}`、验证 session 存在、替换 shell 域中的 session 环境、返回 204。复用现有 `parse_body`, `session_not_found`, `no_content`。
2. 将 session 环境状态放入已拥有 shell 进程生命周期的 `ShellSessions`，而不是新增跨 crate 的全局缓存或 DB 表。它是唯一运行时 authoritative state；`set` 替换整个变量映射，`clear` 随 session 删除执行。
3. 在 `ShellSessions::spawn` 内按 `Owner.session_id` 读取环境，并把覆盖值传到 `process::spawn`；process 层只负责把 borrowed 环境写入 `Command`，不复制到每个 chunk/message 热路径。请求边界和 `Command` 构造时允许一次必要的映射持有/读取。
4. 保留当前 `HERDR_*` 清理规则，并在 session 覆盖基础环境后继续清理这些变量。session 变量覆盖当前进程环境，符合上游 `{...process.env, ...variables}` 语义；额外的终端变量继续由 ox 当前 shell 设计决定，本次不引入新的 API 字段。
5. 不持久化环境变量，不把它们放进事件/`session_v2`，因为上游也是进程内 ephemeral 状态；不做通用环境抽象、workspace 环境、环境 diff/merge 或跨进程同步。

### 模块边界、不变量与所有权

- `crates/ox-server/src/api/session/environment.rs`（新增）：HTTP 编排与 wire schema；不直接 spawn、不保存另一份环境。
- `crates/ox-tools/src/shell/sessions.rs`：session 环境的唯一 owner，与 shell session 表同生命周期；`set_environment` 替换映射，`clear_environment` 幂等，spawn 在 owner session 上应用当前快照。
- `crates/ox-tools/src/shell/process.rs`：OS 进程创建；接收借用的环境视图，负责写入 `Command`，不理解 session/API。
- `crates/ox-server/src/api.rs`：只挂载路由，不承载决策逻辑。
- `crates/ox-server/src/api/session/lifecycle.rs`：删除 session/tree 时清理环境；删除失败时不额外制造不一致状态。

关键不变量：

- 路由仅接受 PUT；成功必须是 204，无响应体。
- 缺少或错误 `variables`、非字符串键值、坏 JSON 返回现有 400 `InvalidRequestError`。
- session 不存在时返回现有 404 `SessionNotFoundError`，且不得写入环境。
- 同一 session 的下一次 PUT 完全替换上一次变量集合；旧键不再传给 shell。
- 未设置 session 环境时 shell 行为与当前实现字节/环境语义一致（仍继承父进程环境并移除 `HERDR_*`）。
- 删除 session 后后续 shell 不再读取已删除 session 的环境。

### 拒绝的替代方案

- **只加路由并返回 204**：会消除表面 404，但模型 shell 和 `session.shell` 仍看不到变量，不满足上游行为。
- **新增 DB 表/事件持久化环境**：偏离上游 ephemeral 语义，引入迁移、隐私和清理复杂度；没有需求证据。
- **在 API handler 中保存独立 HashMap**：会让 API state 与 shell owner 产生双份 authoritative state，且模型工具无法自然读取。
- **把环境塞入 `ToolContext`**：只能覆盖模型工具路径，遗漏 HTTP `session.shell`，并把 HTTP session 状态传播到所有工具执行上下文。
- **修改全局 `std::env`**：跨 session 污染且有并发竞态，违反上游按 session 隔离的语义。

### 验证策略

先写一个最小失败测试，确认当前缺路由/环境传播，再实现：

- 路由/handler：PUT 存在 session 返回 204；未知 session 返回 404；变量缺失/类型错误返回 400。
- 替换语义：设置 `A`、再设置 `B`，shell 看到 `B` 但看不到旧的 `A`。
- 传播语义：session shell 与模型 shell 的共同 `ShellSessions` spawn 都应用 session 环境；无 session owner 的普通 shell不受影响。
- 清理语义：删除 session 后环境被清除。
- 运行 `cargo fmt --check`、目标 crate 测试、workspace `cargo clippy --workspace --all-targets -- -D warnings` 与完整 workspace 测试；Rust 命令均使用 worktree 自有 `CARGO_TARGET_DIR`。

## 实施任务

### 任务 1：实现 session environment 状态与 OS spawn 传播

- **文件**：`crates/ox-tools/src/shell/sessions.rs`、`crates/ox-tools/src/shell/process.rs`，必要时 `crates/ox-tools/src/shell/mod.rs` 调用点。
- **内容**：为 `ShellSessions` 增加按 session 的 ephemeral 环境 owner；扩展 `SpawnRequest`/owner 读取路径；让 `process::spawn` 接收借用环境并通过 `Command` 应用覆盖，同时保留 `HERDR_*` 清理和默认行为。
- **测试**：先添加能复现 session 变量未传播的失败测试，再验证替换和无 owner 回归。
- **依赖**：无。

### 任务 2：实现 HTTP handler、路由与删除清理

- **文件**：`crates/ox-server/src/api/session/environment.rs`（新增）、`crates/ox-server/src/api/session/mod.rs`、`crates/ox-server/src/api.rs`、`crates/ox-server/src/api/session/lifecycle.rs`。
- **内容**：新增 `PUT /api/session/{sessionID}/environment`；严格接收 `variables: Record<string,string>`；先查 session，再替换 `ShellSessions` 环境；删除 session/tree 时清除环境；保持错误和 204 响应契约。
- **测试**：先添加当前缺路由/handler 的失败测试，再覆盖 204、400、404、替换与删除清理。
- **依赖**：任务 1 的 `ShellSessions` API；因此任务 1 完成后执行。

### 任务 3：整体复核与验证

- **文件**：任务 1/2 的全部文件（只读复核，不新增架构）。
- **内容**：对照上游 OpenAPI 和实现逐项检查路径、方法、body、状态码、session 隔离、spawn 传播、删除清理；修复只限本功能范围的问题。
- **命令**：fmt、相关测试、clippy、workspace 测试；记录未覆盖的 workspace-session 特殊语义作为明确偏差。
- **依赖**：任务 1、2。
