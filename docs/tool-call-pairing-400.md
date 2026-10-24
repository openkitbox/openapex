# 严格上游 400：悬空 tool_call（`shell:47`）— 问题分析与 ox 侧修复方案

日期：2026-10-24 · 现象发现于 bluesim 开发会话，切换到 kimi3.1pro（Moonshot 直连）后出现。

## 1. 现象

会话中任意请求，在 kimi3.1pro 模型上稳定返回：

```
cardinal returned HTTP 400: upstream HTTP 400: {"error":{"message":"an assistant message
with 'tool_calls' must be followed by tool messages responding to each 'tool_call_id'.
The following tool_call_ids did not have response messages: shell:47", ...}}
```

其它模型（走其它 provider/协议臂）同一会话正常 —— 说明是**ox 发出的请求历史本身**携带了一个
"悬空调用"：某条 assistant 消息带 `tool_calls`（id=`shell:47`），但其后没有任何
`role:"tool"`、`tool_call_id:"shell:47"` 的结果消息。Moonshot 的 OpenAI 兼容接口严格校验配对，
直接 400；宽松的上游不报错，所以只在 kimi3.1pro 上暴露。

## 2. cardinal 侧证据（为什么 cardinal 不是根因，但可以兜底）

- 同协议请求走**字节透传**，不经翻译：`crates/cardinal/src/translation/mod.rs`
  （"Same-protocol calls never touch this module — byte-faithful pass-through"）。
  ox(Chat) → Moonshot(Chat)，ox 发什么 Moonshot 收什么。
- cardinal 已有同类 400 的补丁 `join_split_tool_call_turns`
  （`src/relay/dialects.rs:264` 起的文档注释记录了 DeepSeek 一模一样的报错文案）：
  它只修"调用与结果被**拆开**"（中间插了 assistant 消息），**不修"结果彻底缺失"**；
  且只在 `fixups.deepseek_dialect` 打开时执行（`src/relay.rs:405`、`src/relay.rs:710`），
  kimi 的 provider 行没开。
- Anthropic 翻译臂会丢弃孤立**结果**（`src/translation/anthropic_messages.rs:379`
  `tool_result.unpaired`），但没有处理孤立**调用**。
- 实锤抓包方法：`~/.ox/config.toml` 的 `[gateway]` 段加 `payloads = true`，复发一次后看
  `~/.ox/payloads/<YYYY-MM-DD>/<HH>.ndjson` 的请求体（`src/body_log.rs:21`），可直接看到
  `shell:47` 挂在哪条 assistant 消息上。

结论：根因在 ox 的历史产出端；cardinal 可选地加一道与 `fill_missing_reasoning` 对称的
`fill_missing_tool_results` 兜底（另行安排，不在本文档范围）。

## 3. ox 侧：悬空调用从哪里来

发送给上游的 wire 消息在 `crates/ox-engine/src/cardinal/wire.rs` 组装
（`assistant_tool_calls()` 产出带 tool_calls 的 assistant 消息，`tool()` 产出结果消息，
`wire.rs:302/311`）。历史折叠在 `crates/ox-engine/src/mctx/`（historian/pass/render/decay）。

`mctx/historian.rs` 的模块注释声称不变量："ox 的 assistant 消息同时携带 tool calls 和它们的
results，完整 tool arc 不会跨越消息边界"，并提到 terminal-arc healing / tool-only gap healing。
但实际有悬空调用到达了上游，说明至少存在一条破坏不变量的路径。候选（按可能性排序）：

1. **回合被打断**：工具调用已发出/已渲染，但结果从未落盘（用户 Esc、发新消息、进程中断）。
   历史里留下"只有调用没有结果"的 assistant 消息。
2. **折叠/渲染路径不一致**：historian 或某条 render 路径（`render.rs`、`decay_render.rs`、
   `pass.rs`）把结果条目折叠掉、却保留了带 calls 的 assistant 消息。本例中 `shell:47`
   处于早已折叠进 `## 摘要` 的历史区间，嫌疑最大。
3. ctx_reduce 的 `[dropped]` 占位机制：设计上应保留消息外壳，需验证没有路径会把 tool 结果
   **消息**整条移除。

## 4. ox 侧修复方案（交给实现 agent）

### A. 发送前配对保障（必做，兜底，provider 无关）

在 `crates/ox-engine/src/cardinal/wire.rs` 的请求组装出口加一次校验/修复：

- 顺序扫描 `messages`；收集每条 assistant 消息的 `tool_calls[*].id`；
- 对照其后所有 `role:"tool"` 消息的 `tool_call_id`；
- 对没有任何结果回应的 id，**就地合成**一条结果消息并插到该 assistant 消息之后：

```json
{"role": "tool", "tool_call_id": "<id>", "content": "[no result recorded: interrupted or folded]"}
```

- 注意与 cardinal 的 `join_split_tool_call_turns` 语义兼容：结果必须紧跟在 call-bearing
  assistant 消息之后，中间不得插入其它消息。
- 该修复让 ox 对**任何**严格上游（Moonshot/OpenAI/Anthropic 等）都安全，且不改变语义
  （模型看到"无结果"可以自行重试）。

### B. 中断回合的落盘修复（根因修复之一）

回合被打断（用户中断/新消息抢先/崩溃恢复）时，若已有 tool call 发出而结果未记录，
落盘时补一条合成结果 part（status=interrupted，content 同上），保证持久化历史永远
不含"裸调用"。

### C. 折叠不变量的审计与测试

- 审计 `mctx/historian.rs`、`render.rs`、`decay_render.rs`、`pass.rs`：任何保留
  assistant-with-calls 的渲染路径，必须同时保留（或重建）其全部结果消息。
- 加一个测试：渲染产物 → 按 wire.rs 同样的规则组装 → 断言每个 tool_call id 都有
  紧随的 tool 结果；用"折叠区间包含带调用的回合"的夹具覆盖（对准本例场景）。

## 5. 验收标准

1. 单测：wire 组装器遇到孤儿调用（构造夹具：折叠区间吞掉结果 / 模拟中断回合）时，
   产出含合成结果消息，且结果紧跟 assistant 消息。
2. 端到端：开启 cardinal `payloads = true`，在含长折叠历史的会话上用 kimi3.1pro 连续
   多轮对话，请求体的 messages 数组中每个 tool_call 均有配对结果，无 400。
3. 回归：现有 mctx/historian 测试全绿；折叠摘要内容不受合成消息影响（合成消息只进
   wire，不进摘要文本）。

## 6. 参考：cardinal 侧可选兜底（另开任务）

- 给 Chat passthrough 增加 `fill_missing_tool_results`（与 `fill_missing_reasoning` 对称，
  含 `*_pending` 只读检测，保持现有 Cow 单拷贝纪律）；
- 挂载范围从 `fixups.deepseek_dialect` 扩大到所有 Chat 上游（或给 kimi/moonshot 开
  dialect fixup）。

---

## 7. 取证结论（2026-10-24 补记）——§3 的三个候选全部排除，本例悬空 id 不是 ox 产生的

对 ses_e965493f7ffeyPAeaNiGw0RWJS（bluesim，模型串始终 k3-256k，故障发生在路由指向
kimi3.1pro 期间）做了全量库副本取证：

1. **`shell:47` 从未存在于 ox 任何持久状态**。全库 25,694 条 `session.tool.input.started`
   事件（append-only，含该会话全部 211 个 tool part）中没有一条 id 是 `shell:47`；
   所有 LIKE 命中（19 条消息 + 28 条事件）全是错误文本的转述。ox 不生成 `名字:序号`
   形状的 id：`settle_tool_calls`（step.rs:939）对空 id 直接跳过，无兜底生成。
2. **ox 发出的请求是完整配对的**。该会话 211 个 tool part 全部 `executed:false` +
   `state:completed/error`（0 个 streaming/running 残留、0 个 executed:true），
   按 `tool_result_message`（step.rs:1756）每个调用都紧跟结果消息。
3. **§3 的三个候选逐条排除**：无中断残留 part（候选 1）；无 compaction checkpoint、
   mctx 折叠不改 tool part（候选 2）；mc_drop 空表（候选 3）。
4. **本地 cardinal（e42db16，与 origin 同步）无 id 改写逻辑**：透传字节保真；
   deepseek 方言 fixup 只并回合不碰 id；翻译臂兜底 id 是 `toolu_*`/`call_*` 形状。
5. 实验旁证：绕过 ox 向 c1.iios.net 直发配对完好的最小请求 → 200（当时路由
   glm-5.3-flash）。

结论：悬空 id 是在 **ox→Moonshot 之间**被制造出来的（远端 cardinal 对该候选的变换，
或 Moonshot 侧问题——其报错文案是 OpenAI chat 形状，但 k3 路由当时指向的
kimi3.1pro 配置无法从本机查看）。**§4-A 的 wire 修复治不了本例**（ox 请求没有可补的
洞），但仍于 38888e5 落地：它封的是另一个真实潜在路径——**进程在工具执行中途崩溃/
被杀**时 part 停在 `Streaming/Running`，wire 会发出无结果的调用（§3 候选 1 的崩溃
变体，即 subagent「崩溃恢复明确不做」的已知空洞）。全 workspace 测试通过。

### 7.1 待办：根因定案需要 cardinal 侧证据（二选一）

- **(a)** 提供 c1.iios.net 的 cardinal admin key 或 ssh：查 payloads（body_log）里
  400 当时的请求体 + kimi3.1pro 候选的 provider 配置（protocol / fixups 开关）；
- **(b)** 把 k3-256k 路由临时指回 kimi3.1pro，在本机对 ox→cardinal 段做透明字节
  捕获（本地转发器或本地 cardinal payloads=true）——一次复发即可定案：预期 ox
  出站字节配对完好，责任在 cardinal→Moonshot 段。
