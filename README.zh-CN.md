# ox

[English](README.md) · **简体中文**

OpenApex 是 opencode v2 聊天协议的独立 agent 实现，使用 OpenCode 作为
UI：OpenCode 负责面向用户的交互，OpenApex 提供 agent 运行时、会话引擎、工具
和 provider 接入。一个二进制提供 HTTP/SSE **服务端**，以及用于编辑器集成的
**ACP agent**。

ox 用自己的数据库和配置，都在 `~/.ox` 下；它既不读也不写 Bun 实现的那套文件。

## 环境要求

- 支持 edition 2024 的 Rust（1.85 及以上）
- macOS 或 Linux（服务端会绑定 unix socket，所以不支持 Windows）

## 快速开始

```bash
cargo build --release

./target/release/ox          # HTTP 服务端，默认 127.0.0.1:8787
./target/release/ox serve    # HTTP 服务端，默认 127.0.0.1:8787
./target/release/ox acp      # 走 stdio 的 ACP agent，给编辑器用
```

## OpenCode 集成

OpenApex 使用 OpenCode 作为 UI。启动 OpenApex HTTP 服务端后，将 OpenCode
指向兼容的 API endpoint；OpenCode 负责会话交互和展示，OpenApex 负责 agent
执行、会话、工具和 provider 接入。

第一个参数决定模式。以 `-` 开头的参数、或者干脆没有参数，都启动 HTTP 服务端。

| 模式 | 命令 | 行为 |
|---|---|---|
| 服务端 | `ox [flags]` 或 `ox serve [--stdout\|--no-stdout] [--log-file PATH] [--exit-when-idle]` | 跑 HTTP/SSE API 和会话引擎。每个 home 只允许一个服务端：它拿实例锁，并绑定私有 unix socket。`--exit-when-idle` 让它在没有任何客户端持 lease 后退出。 |
| ACP agent | `ox acp [--server URL] [--log-file PATH] [--no-stdout]` | 通过 stdio 使用 ACP v1。不指定 `--server` 时，先尝试本地 Unix socket，再尝试配置的 TCP 地址，最后才在进程内启动服务端。指定 `--server` 时只连接该地址。日志走 stderr 或 `--log-file`。 |

## 文件位置

| 路径 | 用途 |
|---|---|
| `~/.ox/ox.toml` | 配置 |
| `~/.ox/ox.db` | 数据库（turso，兼容 SQLite） |
| `~/.ox/ox.log` | 后台服务端的日志 |
| `~/.ox/run/ox.sock` | 私有 unix socket（0600） |
| `~/.ox/run/ox.lock` | 单实例锁 |

## 配置

`~/.ox/ox.toml` 是可选的，每一段都有默认值。`OX_DB_PATH`、`OX_LISTEN`、
`OX_SUBAGENT_DEPTH` 三个环境变量覆盖同名设置。

`listen`、`db_path`、`default_model` 这类顶层键，与 `[providers]`、`[models]`、
`[context_caps]`、`[roles]`、`[agents]`、`[magic_context]`、`[mcp]`、
`[permissions]`、`[subagent]`、`[events]`、`[sessions]`、`[tmp]`、`[turn]`
这些段并列。

### Provider

一个 provider 就是一个出站端点加一个密钥。`base_url` 填**版本后缀之前**的部分
——协议路径由 ox 拼接（Chat Completions 用 `/v1/chat/completions`，
Anthropic Messages 协议用 `/v1/messages`）：

```toml
default_model = "my-gateway/some-model"

[providers.my-gateway]
base_url = "https://gateway.example.com"
api_key = "…"
# protocol = "chat_completions"   # 或 "messages"；默认 chat_completions
```

模型 id 用端点自己模型列表里报的名字，写成 `provider/id` 的形式。少数厂商有一张
内置表提供端点和协议，所以 `[providers.<厂商>]` 段可以只写 `api_key`。

`[models.<id>] candidates` 把一个逻辑模型变成有序的故障转移链，第一个成功打开流的
端点服务这一轮：

```toml
[models.fast]
candidates = [
  { provider = "my-gateway", model = "some-model" },
  { provider = "backup", model = "some-model" },
]
```

更早的 `cardinal_url` / `cardinal_key` 两个键仍然描述那个隐式默认端点，路由可以用
它的 provider 名引用它。

### 模型角色

有三类请求不属于会话本身的这一轮，可以各自指定逻辑模型。值必须是模型目录认识的
逻辑 id（`[models.<id>]` 的路由键，或单端点模式下的 wire id）：

```toml
[roles]
title = "slim"        # 默认就是逻辑模型 `slim`
# compaction = "big"  # 默认跟随会话模型（/slim 那一轮则是该轮模型）

[agents.explore]
model = "slim"        # 默认值来自内置 agent 表
```

目录里没有 `slim` 时，标题生成回退到会话模型；`/slim` 只让下一条请求走 `slim`，
不改变会话。子会话的 agent 指定了模型却查不到目录里时，它会直接报错，而不是静默
用父会话的模型。

### MCP

在 `~/.ox/ox.toml` 中配置本地和远程 MCP 服务器：

```toml
[mcp.servers.local-example]
type = "local"
command = ["my-mcp-server", "--stdio"]
disabled = true

[mcp.servers.remote-example]
type = "remote"
url = "https://mcp.example.com/mcp"
disabled = true
```

`disabled` 默认为 `false`。设为 `true` 后，启动和重载配置时会跳过该服务器。
手动连接是临时覆盖；下次成功重载配置或重启服务端后，重新按配置决定是否连接。
修改配置后重启服务端生效。

MCP 连接和工具注册表是进程级的，不按会话隔离。连接或断开服务器会影响所有会话；
工具以服务器名作为前缀暴露。

## 编辑器集成

把支持 ACP 的编辑器（Zed、JetBrains 等）的 agent 命令指向 `ox acp`。工作目录由
编辑器按会话给出；ox 把 ACP 会话一对一映射到 ox 会话，把自己的 agent 作为 ACP
模式暴露，并把助手正文、思考内容和工具调用作为 session update 流式推回去。

和普通 CLI 包装不同，`session/load` 会重放已存的 transcript，包括 magic-context
的丢弃内容覆盖层，所以编辑器看到的就是模型当时看到的那份。ox 无法保证隔离的
会话设置会以 ACP invalid params 错误拒绝，且在创建或改动任何会话之前失败 ——
绝不静默丢弃。MCP 服务器统一配在 `~/.ox/ox.toml` 的 `[mcp.servers.<名字>]` 里，
属于进程级配置；编辑器不能按会话添加。

### ACP v1 稳定能力矩阵

`ox acp` 是走 stdio 的 ACP v1 agent。它的稳定 wire 面有意小于完整
SDK schema。不启用、不协商任何 `unstable` ACP 能力；SDK 自身的传递
`unstable_proxy_chains` schema feature 不代表 ox 的能力。

**已实现**

| 范围 | 稳定能力 |
|---|---|
| 初始化 | `initialize`；返回 ACP v1，声明 `session/load`，并声明会话 list、delete、resume、close 能力。 |
| 会话生命周期 | `session/new`、`session/load`、`session/resume`、`session/list`、`session/delete`、`session/close`。 |
| 会话控制 | `session/prompt`、`session/cancel`、`session/set_config_option`、`session/set_mode`。 |
| 选择 | 模型、模式和思考强度的 `configOptions`；不实现 `session/set_model`。 |
| Prompt 内容 | 文本、图片、文本型 embedded resource。二进制 embedded resource、音频和 resource-link prompt 会拒绝，不会隐式下载。 |
| 会话更新 | `content/tool/update` 映射：助手正文和思考增量、用户消息重放、工具调用及工具进度/结果、工具输出中的 resource link、仅在上游给出真实 `used`/`size` 时的 `usage_update`，以及按能力门控的 notice/compaction 更新。 |
| 客户端交互 | permission 请求会桥接到客户端。只有客户端声明稳定的 form elicitation 能力时才使用 elicitation；否则取消表单并 fail-closed。 |

**Fail-closed（ox 无法兑现的请求设置）**

| 请求 | 行为 |
|---|---|
| `session/new` / `session/load` / `session/resume` 上的 `systemPrompt`、`additionalDirectories` | 在任何 HTTP session 操作之前以 invalid params 拒绝，绝不静默丢弃。 |
| 同样三个方法上的按会话 `mcpServers` | 在创建、加载或恢复任何 session 之前以 invalid params 拒绝：ox 无法证明工具注册表按会话隔离。 |
| 客户端未声明 form elicitation 时的 elicitation | 取消待处理表单并 fail-closed，绝不自动放行。 |
| 客户端 filesystem（`fs`）/ terminal 回调 | 不声明，ox 也不发起这类请求。 |

**明确排除**

- ACP v2，以及所有 `unstable` ACP 能力族（proxy chains、fork、archive/unarchive、
  set_title、status 等）。
- MCP-over-ACP 与 HTTP/SSE MCP；不声明 `mcpCapabilities.http`/`sse`/`acp`。
  `~/.ox/ox.toml` 中配置的 MCP 仅是进程级能力。
- HTTP/SSE ACP transport：`ox acp` 后端的 HTTP/SSE 服务是实现细节，不是 ACP transport。
- provider/connect。
- 二进制 embedded resource、音频和 resource-link prompt 会被拒绝，不会隐式下载。

## 仓库结构

| crate | 职责 |
|---|---|
| `ox`（根） | `ox` 二进制，外加一层 re-export 所有 crate 的 façade |
| `ox-schema` | opencode v2 协议的 serde 模型，以及共享路径 |
| `ox-db` | 存储：turso 连接、PRAGMA、版本化 schema、单写者闸门 |
| `ox-llm` | 端点、凭据、模型目录、两套聊天协议和 SSE 分帧 |
| `ox-core` | 配置、事件总线及其 projector |
| `ox-engine` | 按会话的 runner、magic context、系统提示词 |
| `ox-tools` | 内置工具集：从上游移植的文件与 shell 工具，外加子会话、todo、skill 和检索 |
| `ox-mcp` | MCP 客户端 hub |
| `ox-server` | HTTP API、实例锁与生命周期 |
| `ox-acp` | 给编辑器用的 ACP 翻译层 |

依赖是单向的。`ox-schema` 是大家一起用的契约；`ox-llm` 只依赖它、不依赖 ox 的其它
部分；`ox-core` 在其上加了存储和事件总线；引擎与工具集再往上；`ox-server` 负责装配。
`ox-acp` 是客户端 —— 它只说公开的 HTTP 契约，不伸手进 runner。

## 开发

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo build --release
```

质量闸门是 `cargo clippy --workspace --all-targets`：这个仓库有意不带自动化测试，
所以 clippy 干净就是标准。

## 文档

- [Skills](skills/README.md) —— 项目 skill 的加载
