# ox

[English](README.md) · **简体中文**

opencode v2 聊天协议的纯 Rust 实现。一个二进制，三种模式：HTTP/SSE
**服务端**、终端 **TUI**，以及让编辑器直接驱动 ox 的 **ACP agent**。

ox 用自己的数据库和配置，都在 `~/.ox` 下；它既不读也不写 Bun 实现的那套文件。

## 环境要求

- 支持 edition 2024 的 Rust（1.85 及以上）
- macOS 或 Linux（服务端会绑定 unix socket，所以不支持 Windows）

## 快速开始

```bash
cargo build --release

./target/release/ox          # TUI：连上正在跑的服务端，没有就自己拉一个
./target/release/ox serve    # HTTP 服务端，默认 127.0.0.1:8787
./target/release/ox acp      # 走 stdio 的 ACP agent，给编辑器用
```

TUI 要能回答提问，得先配一个 provider —— 见 [配置](#配置)。

## 三种模式

第一个参数决定模式。以 `-` 开头的参数、或者干脆没有参数，都进入 TUI。

| 模式 | 命令 | 行为 |
|---|---|---|
| TUI | `ox [--server URL] [--directory DIR] [--session ID]` | 通过 `~/.ox/run/ox.sock` 连服务端；没有服务端在听就自己拉起 `ox serve --exit-when-idle`，并持有一个 lease，让它在最后一个客户端退出后收尾。给了 `--server` 就走纯 HTTP，不拉进程、不持 lease。 |
| 服务端 | `ox serve [--stdout\|--no-stdout] [--log-file PATH] [--exit-when-idle]` | 跑 HTTP/SSE API 和会话引擎。每个 home 只允许一个服务端：它拿实例锁，并绑定私有 unix socket。`--exit-when-idle` 让它在没有任何客户端持 lease 后退出。 |
| ACP agent | `ox acp [--server URL] [--log-file PATH] [--no-stdout]` | 在 stdin/stdout 上以换行分隔的 JSON-RPC 说 Agent Client Protocol。`GET /api/info` 有响应就挂到已在跑的服务端，否则在进程内用一个临时回环端口自托管服务端。stdout 专供协议用，所以日志走 stderr 或 `--log-file`。 |

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

`[mcp.servers.<名字>]` 在启动时连上一个 MCP 服务器，把它的工具以服务器名作为前缀
暴露出来。连接是进程级的，所以每个会话看到的工具注册表都一样。

## 编辑器集成

把支持 ACP 的编辑器（Zed、JetBrains 等）的 agent 命令指向 `ox acp`。工作目录由
编辑器按会话给出；ox 把 ACP 会话一对一映射到 ox 会话，把自己的 agent 作为 ACP
模式暴露，并把助手正文、思考内容和工具调用作为 session update 流式推回去。

有两处和普通 CLI 包装不同：

- ACP 会话里请求的 MCP 服务器会被忽略并打一条警告 —— ox 的工具注册表是进程级的，
  MCP 服务器统一配在 `~/.ox/ox.toml` 的 `[mcp.servers.<名字>]` 里。
- `session/load` 会重放已存的 transcript，包括 magic-context 的丢弃内容覆盖层，
  所以编辑器看到的就是模型当时看到的那份。

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
| `ox-tui` | 终端客户端 —— 见[它自己的 README](crates/ox-tui/README.md) |
| `ox-acp` | 给编辑器用的 ACP 翻译层 |

依赖是单向的。`ox-schema` 是大家一起用的契约；`ox-llm` 只依赖它、不依赖 ox 的其它
部分；`ox-core` 在其上加了存储和事件总线；引擎与工具集再往上；`ox-server` 负责装配。
`ox-tui` 和 `ox-acp` 是客户端 —— 它们只说公开的 HTTP 契约，不伸手进 runner。

## 开发

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo build --release
```

质量闸门是 `cargo clippy --workspace --all-targets`：这个仓库有意不带自动化测试，
所以 clippy 干净就是标准。

`Taskfile.yml` 包住了常用流程：`task clippy`（fmt 加 clippy）、`task prod-build`
（带 `-C target-cpu=native` 的 release 构建）、`task prod`（该构建装到
`/opt/bin`）、`task tui`（release 版 TUI，指向正在跑的服务端）。

## 文档

- [TUI](crates/ox-tui/README.md) —— 按键、布局、demo
- [Skills](skills/README.md) —— 项目 skill 的加载
