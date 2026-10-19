# 隐私脱敏 —— 从 cardinal 移交的实现规格

> **状态：已决策，未实现。** 2026-10-19 裁定：私密信息处理不做在 cardinal，全部移到 ox 内部。
> 本文是实现规格 + 参考实现指针，交给后续实现 agent 执行。

## 参考实现（cardinal 分支）

**`openkitbox/cardinal` @ branch `privacy-redaction`（commit `2df7042`，已推送 origin）**
在 main 上已被移除；main 只保留了与此无关的上游密钥回显修复（`166d7c6`）。

| 文件 | 行数 | 内容 |
|---|---|---|
| `src/privacy/mod.rs` | 722 | mask / restore / scrub 入口、**20 个永久回归测试** |
| `src/privacy/rules.rs` | 396 | 14 条规则（私钥块 / 各家 API key / JWT / URL 凭据 / secret 赋值 / 邮箱 / 身份证 / 手机 / 银行卡+Luhn）+ 边界约束 |
| `src/privacy/placeholders.rs` | 415 | HMAC 占位符、持久加密再掩索引、请求级还原映射 |
| `src/privacy/stream.rs` | 257 | 流式还原事件保持状态机（跨帧重组） |
| `src/protocol/mod.rs` (+227) | | 渲染时递归写器 hook —— **ox 不需要移植这层**（见下） |
| `src/body_log.rs` (+128) | | payload NDJSON 保留期清扫（cardinal 本地卫生，可选取回） |

## ox 侧落点

| 关注点 | cardinal 参考 | ox 落点 |
|---|---|---|
| 发送前掩码 | 渲染时递归写器遍历线上 JSON | `src/cardinal/wire.rs` 序列化路径，遍历**类型化** `ChatRequest`：只处理 Text / tool result 内容 / ToolUse input 里的字符串 |
| 响应还原 | 非流式 + 跨帧状态机 | `src/cardinal/stream.rs` `Assembler` **组装后的完整文本上一次性还原**（单一出口） |
| 落盘保护 | payload NDJSON 不可逆 scrub | `src/db/projector.rs` —— **待决策**：存占位符 + 渲染时还原（改数据模型，建议单独立项） |
| 配置 | `[gateway.privacy]` 默认关 | ox config 加 `[privacy]`，默认关，key/index 路径可配 |

**流式注意**：若 UI 在组装前逐 delta 渲染，占位符可能被切开（cardinal 实测发生过：`{{API_KEY_sfkoq7dr}` + `} ok` 两帧）。两条路：渲染只吃组装后文本（推荐，零新机制）；或移植 `stream.rs` 的按事件保持逻辑。

## 为什么 ox 侧便宜一个量级（别把 cardinal 的复杂度搬过来）

cardinal 坐在协议无关中继上，看到的是裸 JSON，因此需要：`privacy::opaque()` 靠兄弟键猜签名块、结构字段跳过清单、4 协议内核 × 双侧对称、跨帧字节状态机。**这些在 ox 全部不存在**：

- `ChatMessage` / `AssistantContent` 是枚举 —— thinking 签名块由类型系统天然排除，无猜测；
- 只有 OpenAI Chat 一种协议 —— 无翻译路径；
- `Assembler` 已解决跨 chunk 重组 —— 还原发生在完整文本上，不是字节流上。

cardinal 那套机制是「位置错误付出的学费」，不是要继承的资产形状。

## 必须逐字保留的机制（invariants，即移植规格）

1. **占位符**：`{{KIND_xxxxxxxx}}`，`xxxxxxxx = base32(HMAC-SHA256(k, kind+value))[:8]`。同一值恒得同一占位符（跨轮、跨重启）—— 这是 prompt cache 与多轮再掩的根基。
2. **HMAC key**：`~/.ox/privacy.key`（持久，别每次启动重生成）。
3. **持久再掩索引**：`~/.ox/privacy.index`，AEAD 加密，**只存 `HMAC指纹 → 占位符`，永不存明文**。启动加载；命中 = 裸明文重放也换回同一占位符。这是唯一防泄漏的关键状态（进程内存版曾实测泄漏：重启后 `curl -u admin:Tr0ub4dor` 明文直达上游）。
4. **还原映射是请求级的**：placeholder→value 只在单请求内存活，随请求消亡。
5. **`data:` URI 永不掩**（base64 里 `13800138000` 会误中 PHONE）。
6. **转义规则**：只有 JSON-in-JSON 字段（tool `arguments` 等）需要字符串层转义还原；其余走 serde 再序列化、恰好转义一次。round-trip 必须字节精确（含引号/反斜杠/换行的多行私钥）——cardinal 在这里出过真 bug，有测试 `round_trip_preserves_escapes`。
7. **幂等**：mask(mask(x)) == mask(x)；restore 同理；本身形如占位符的值跳过。
8. **结构字段跳过**：`model` / `type` / `role` / `*_id` / `finish_reason` 等不做词匹配。
9. **20 个测试就是验收标准**：`cargo test --lib`，全部移植（在分支 `src/privacy/mod.rs`）。

## 已接受的残留（ox 实现时知悉即可）

- 长十六进制串里的 11 位数字段可误中 PHONE（低频，BANK_CARD 有 Luhn 保护）；
- `words` 自定义词是未锚定子串匹配；
- 非 JSON 响应体原样穿过（ox 侧响应恒为 JSON，此条大概率自然消失）。

## 覆盖面变化（已知并接受）

cardinal 直连客户端（opencode TUI、claude code、curl）不再被脱敏覆盖 —— 裁定接受：脱敏是 agent 的职责，不是网关的。

## 延期项：本地 ML 分类器（自由文本 PII，2026-10-19 评估过，不做）

正则只能抓**有形状**的（前缀、校验位、语法）；人名/地址/日期只有**语义**，正则结构性做不到。参考 AstrLink（`cardinal/reference/AstrLink`，自研 zh+en token 分类器，HF `QuantumNous/astrlink-guard`）：

- BERT 系 token 分类 → BIO 解码 → 偏移对齐；10 类（含 PERSON/ADDRESS/DATE）；int8 38.9 MB；ONNX Runtime sidecar；8 KiB 批 + 512 B 重叠；512 token 窗口；segment 级缓存。
- **关键语义**：它是检测器的**替换**不是融合（policy Mode 二选一）—— 直接换会丢私钥/JWT 等正则独有覆盖，必须自建 regex ∪ model 并集。
- antb-tube-2 实测可跑（AVX2、2 GB 内存可用 1.36 GB），但 +268 MB RSS、长 prompt 秒级延迟。
- 完整评估存 cardinal 仓 `.opencode/plans/privacy-redaction.md`（机器本地，未入库）。
- **重拾条件**：仅当需求从「凭据安全」扩到「自由文本个人身份识别」。
