---
name: opencode-parity
description: 对比上游 opencode 的 server API 与 ox 的差异并推动对齐。用于：上游发新版本后评估协议变化；对齐具体 endpoint 或 HTTP 行为；检查 ox 是否漏了 API endpoint、请求字段、响应 schema 或 SSE 事件。
---

# opencode-parity

把上游 opencode 当作行为规范，找出 ox 与它的差异。对齐的是**用户可见的行为**，不是上游的代码结构。

- 上游源码：`/Users/john/github/opencode`（也可经 `.ox/reference/agents/opencode` 访问）。协议在 `packages/protocol/openapi.json`。
- ox server：`crates/ox-server/src`（路由 `api.rs` 的 `build_router`）。

## 选场景

| 要做的事 | 走哪条 |
|---|---|
| 上游升版，问“协议要跟进什么” | A：升版 |
| 对齐一个具体 server API 行为 | B：单功能 |

## 场景 A：升版

在本目录下执行：

1. `python3 openapi_diff.py OLD_TAG NEW_TAG`：协议层有哪些 path、operation、参数、schema 变化。
2. 对每个新增或变化的 endpoint，按场景 B 评估并更新 server 实现与记录。
3. 完成后记录当前上游基线。

## 场景 B：单功能

1. `rg` 上游协议与 ox server 源码找对应 endpoint。
2. 读取两侧代码，逐项核对状态、输入、字段、边界和副作用。
3. 用固定格式输出缺口表：`缺口 | 上游 file:line | ox file:line | 用户可见 | 体量 | 决策`。
4. 实施对齐走项目的开发流程（计划文档 + worktree）。

## 台账命令

- `python3 openapi_diff.py OLD_TAG NEW_TAG`：比较两个上游 OpenAPI 版本。

## 硬规则

1. 两侧结论都带 `file:line`，不凭印象。读不到的就写“未核实”。
2. 对齐行为，不对齐架构。
3. 服务端只比较 API endpoint。

## 文件

- `openapi_diff.py`：两个上游 tag 的 `openapi.json` 差异。
- `tests/test_openapi_diff.py`：差异脚本的单元测试。
