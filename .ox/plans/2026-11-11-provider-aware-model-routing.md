# provider-aware model routing

## 分析与方案

### 根因

`ModelRef` 正确保存了 `id + provider_id`（`crates/ox-schema/src/support.rs:194-200`），模型切换也按两者比较（`crates/ox-server/src/api/session/switch.rs:184-188`），但主请求在 `crates/ox-engine/src/runner/step/request.rs:187-193` 只把 `model.id` 写入 `ChatRequest`。`EndpointRouter`/`Routes` 随后只能按裸 id 猜 provider；`Routes::resolve`（`crates/ox-llm/src/config.rs:349-372`）先命中 builtin 的 `owns("deepseek-flash")`，所以 `cardinal/deepseek-flash` 会被 deepseek builtin 抢走。

同一边界问题还会污染同 id 跨 provider 的运行时数据：
- `ModelCatalog::window_of` 只按 id 查（`crates/ox-llm/src/model_catalog.rs:332-339`），API 投影和 compaction geometry 会取错 provider 的 context；
- `request_budget::apply` 的 calibration key 只使用裸 id（`crates/ox-engine/src/runner/request_budget.rs:142-145`），stream usage 也只传裸 id（`crates/ox-engine/src/runner/step/stream.rs:296-299`）；
- mctx budget/head/preview 及 historian prefix cache 使用裸 model id（`crates/ox-engine/src/mctx/mod.rs:25-40`、`crates/ox-engine/src/runner/step/context.rs:26-100`、`crates/ox-engine/src/runner/historian_trigger.rs:1078-1082`）；
- `historian_model` 明确把 `provider/id` 截成 id（`crates/ox-engine/src/mctx/historian.rs:2640-2668`），使隐藏 completion 也无法保留 provider。

### 设计决策

1. **provider 是路由身份，不是 wire model 名。** 由于 `ChatRequest` 是 public struct，增加 required 字段会破坏既有 struct literal compatibility，因此不在请求结构中加入 provider；改用 `Chat::stream_chat_for(request, provider)` 的 out-of-band seam，在进入 `EndpointRouter` 时使用 `(provider, model)`。HTTP wire 仍只发送裸 `model`，不把 `provider/id` 拼进 wire，保持 cardinal/各 provider 的协议契约和现有字节格式。
2. **显式 provider 优先于 builtin 名称推断。** `Routes::resolve(provider, model)` 的顺序为：匹配的 `[models.<logical>]` route → 已指定 provider 的 direct endpoint → 无 provider 时 builtin ownership → legacy cardinal passthrough；指定了不存在的 provider 必须 `NoRoute`，不能降级到另一个 endpoint。对 `cardinal/deepseek-flash`，直接返回 cardinal candidate。
3. **显式 `[models]` 路由仍是逻辑模型的权威。** 当请求没有 provider 时按原 route；有 provider 时只有 route 首 candidate provider 与所选 provider 一致才使用该 route，否则尊重所选 provider。这避免同名不同 provider 的选择被逻辑 alias 劫持。
4. **目录和预算按 `(provider,id)` 精确匹配。** 新增 `ModelCatalog::window_of_for(provider, id)` 和 provider-aware `runtime_limits`; 保留裸 `window_of(id)` 仅作为兼容 API 投影入口，但所有已有 `ModelEntry` 调用改传 provider。`context_caps` 继续以客户端看到的逻辑 id 为 key，不增加 speculative provider-specific 配置格式。
5. **calibration 与 mctx 身份同步。** calibration 的内部 key 由 provider+id 构成；主步骤、generate、compaction、title、historian/dreamer 都以 `ModelRef` 传递 provider，通过 `Chat::stream_chat_for` 进入 transport。`ModelChain` 从字符串改为持有 `ModelRef`，只在构造 `ChatRequest` 时取 `.id`；prefix cache 比较 provider 和 id，禁止跨 provider 复用。
6. **最小改动与所有权。** provider 通过 `stream_chat_for` 在调用边界借用传递，不复制 transcript/messages，不增加新 trait 或缓存。路由 candidate 只有在改写 wire model 时复制现有 request boundary 已经复制的结构；mctx/calibration 使用借用的 `ModelRef`，只有 detached historian chain 在必须跨生命周期时拥有其小型 `ModelRef`。

### 模块边界、不变量

- `ox-llm/config.rs`：纯路由决策；输入 provider/id/config，输出 borrowed `Targets`；不做 IO。
- `ox-llm/cardinal/router.rs`：端点 candidate 装配和请求尝试；不得重新按模型名前缀猜 provider，`stream_chat_for` 的 provider 参数是唯一选择依据（缺失时才走兼容 fallback）。每个 `Chat` decorator（当前为 `LimitedChat`）都必须原样转发该 provider；wire 请求保持不变。
- `ox-llm/model_catalog.rs`：目录 metadata 与 physical limits；同一 provider 内同名可去重，跨 provider 不可混用；API/runner 取 window 必须指定 provider。
- `ox-engine` runner/mctx：`ModelRef` 是逻辑模型的唯一身份；wire 只发送裸 id，内部 request provider 必须与 ModelRef 一致；预算、calibration、prefix reuse 必须使用相同身份。

### 被拒绝的替代方案

- 仅删除 builtin `owns()`：会把“走错 endpoint”变成 builtin 模型无法选择/无法路由，且没有修复预算、calibration、prefix cache 的同名串扰。
- 把 `provider/id` 发送给 cardinal：cardinal 的当前协议路由要求裸 model id，且会破坏各 provider 的 wire contract；provider 应留在内部 transport metadata。
- 让 router 再次根据名字优先级猜端点：这正是本 bug 的根因，不能作为长期兼容策略。
- 为每个 provider 建独立缓存/配置层：目录已有 provider+id metadata，额外 cache 会制造漂移；只改 key 精确匹配。

### 测试策略

先写回归测试并确认在旧代码上失败，再实现：
- provider-aware route：`cardinal/deepseek-flash` 明确得到 cardinal，`deepseek/deepseek-flash` 得到 builtin，未知 provider 得到 `NoRoute`；无 provider 的 legacy deepseek 名称仍保持原 builtin 行为。
- catalog：相同 id 的两个 provider 的 `window_of_for` 和 runtime limits 不串；API projection 使用对应 provider window。
- engine/model identity：calibration 在 provider 切换时重新采样；prefix cache 只有 provider+id 都相同才复用；请求构造将 provider 留在内部而不出现在序列化 JSON。
- 运行现有 workspace tests，并用 `cargo fmt -- --check`、`cargo clippy --all-targets --all-features -- -D warnings` 验证。

用户已确认按 provider-aware 方向修复；以下实施在独立 worktree 进行，不重启当前运行服务、不部署二进制。

## 实施任务

### 1. LLM 路由与目录身份

- **文件**：`crates/ox-llm/src/config.rs`、`crates/ox-llm/src/cardinal/router.rs`、`crates/ox-llm/src/model_catalog.rs`、必要的 `crates/ox-llm/src/*` 单元测试。
- **依赖**：无。
- **内容**：给 `Routes::resolve` 增加可选 provider 输入；指定 provider 时直接选择对应 endpoint，未知 provider fail-closed；让 `EndpointRouter` 保存/查找所有 direct endpoint candidate，并将 `stream_chat_for` 的 provider 传入解析；给 `runtime_limits`/context window 增加精确 provider+id 查找；修正 API-facing model window 调用和同源测试。保持 cardinal wire model 为裸 id。
- **验收**：新增测试先在旧代码上失败；目标测试、fmt、clippy 通过；现有无 provider 路由行为不变。
- **提交**：`fix(llm): honor selected provider in model routing`。

### 2. Engine provider 贯通与预算隔离

- **文件**：`crates/ox-engine/src/runner/step/request.rs`、`runner/step/context.rs`、`runner/step/stream.rs`、`runner/step/usage.rs`、`runner/request_budget.rs`、`runner/drain.rs`、`runner/generate.rs`、`runner/title.rs`、`runner/compaction.rs`、`mctx/mod.rs`、`mctx/calibration.rs`、`mctx/historian.rs`、`mctx/dreamer/scheduler.rs`、`runner/historian_trigger.rs` 及对应现有测试。provider 通过 `Chat::stream_chat_for` 的 out-of-band seam 传递，不修改 public `ChatRequest` 或 wire；每个 decorator 必须转发 provider。
- **依赖**：任务 1 的 API 完成。
- **内容**：所有从 `ModelRef` 构造的主/辅助调用通过 `Chat::stream_chat_for` 设置 provider；calibration、window/budget、usage observation 使用 provider+id；historian/dreamer 的配置和 fallback chain 保留 `ModelRef`；prefix cache 比较完整身份；不复制消息正文，不改变 public `ChatRequest` 或 wire JSON。
- **验收**：新增回归测试先失败后通过；主步骤、title、generate、compaction、historian/dreamer 的请求都能到选中 provider；provider 不出现在序列化请求；workspace fmt/clippy/tests 通过。
- **提交**：`fix(engine): preserve provider identity through model requests`。

### 3. 全局评审与交付

- **文件**：任务 1、2 的全部变更。
- **依赖**：任务 1、2 及其评审均通过。
- **内容**：general reviewer 与 rust-expert 并行审查完整 diff；修复发现并重新评审；worktree 全量验证；rebase 默认分支；主分支 squash 为一个 conventional commit；再跑合并结果验证，删除 worktree 和分支。
- **验收**：main 干净（仅保留本任务意图内的最终提交），测试/fmt/clippy 全绿；不执行部署或重启。

### 实施状态与偏差台账

- 状态：实现提交 `e2a0b10`、`6000fb3`、`bff6e40`、`50419de`、`644404e`、`ffff326` 均已完成；本次补充 explicit logical route alias、direct selected-provider 和未知 provider 的 `stream_chat_for` 回归覆盖，并补充 historian prefix identity gate 测试。
- 验证：最终 workspace 测试 `60 passed`（本任务新增 2 个测试，此前基线为 58）；`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`git diff --check` 均通过。测试 recorder 捕获 `serde_json` serialized request，并断言 selected provider 与 wire model；未引入 live TCP/HTTP smoke，因为项目没有足够简单且不扩依赖的生产 HTTP 测试路径，本台账明确不将 recorder 测试冒充 live smoke。
- 偏差：设计采用 `Chat::stream_chat_for(request, provider)` trait seam，而非修改 public `ChatRequest`，因此 provider 仍是内部 transport metadata；wire 仍保持裸 model。`CardinalClient` 是 terminal transport，负责最终协议请求，不重复执行路由；`LimitedChat` forwarding fix 已由 `50419de` 完成，确保 decorator 原样转发 provider。
- Prefix gate：historian 只有在 provider 与 model id 都相同时才复用 prefix；`same_model` 使用 borrowed provider/model 输入与 `ModelRef` 字段比较，不复制正文，跨 provider 的同名模型明确走 flat shape。
- 补充修复：catalog-less session fallback 曾硬编码 `provider_id = "opencode"`，与 `Routes::resolve` 的 fail-closed provider 校验冲突；已改为 `ox_llm::config::DEFAULT_PROVIDER_NAME`（`cardinal`），恢复无 `[providers]`/无 catalog 的隐式 endpoint passthrough，同时保留显式配置下未知 provider 的 `NoRoute`。回归测试覆盖两种行为。
- 交付：未执行部署、重启或生产服务变更；live HTTP smoke 未做。
