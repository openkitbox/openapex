# ox

**简体中文** · [English](README.md)

## 项目简介

ox 是一个独立的纯 Rust opencode v2 兼容 agent runtime。它实现 opencode v2 聊天协议，
使用 OpenCode 作为 UI：OpenCode 负责面向用户的交互，ox 负责 agent 执行、会话、
工具和 provider 接入。

一个二进制提供两种模式：HTTP/SSE **服务端**，以及供编辑器集成的 **ACP agent**。
ox 使用自己的数据库和配置，默认都位于 `~/.ox`；它不读取或写入 Bun 实现的那套文件。

## 功能

- **原生 provider 路由**：通过 `[providers.<name>]` 配置端点，通过
  `[models.<id>]` 配置有序候选链；流打开前支持故障转移。
- **MCP**：连接本地 stdio 与远程 HTTP MCP 服务器，将工具、资源和指令接入会话。
- **Automations 与 MCP Events**：按时间或 MCP 事件触发 prompt、shell 和 code action，
  支持 push、poll 与经过 HMAC 校验的 webhook 事件投递。
- **Magic context**：上下文折叠、项目记忆、检索和会话历史覆盖层。
- **内置 agent**：默认 primary agent `apex`，以及 `general`、`explore`、`review`、
  `rust-expert` 子 agent。
- **编辑器集成**：通过 stdio 提供 ACP v1，复用 ox 的会话、transcript 和工具执行路径。
- **安全控制**：逐工具的 allow/ask/deny 策略；无效配置重载会保留当前运行版本。

## 环境要求

- 支持 edition 2024 的 Rust（1.85 及以上）
- macOS 或 Linux（服务端会绑定 unix socket，所以不支持 Windows）
- 真实模型调用需要配置 provider
- OpenCode 可作为 UI；编辑器集成则需要支持 ACP 的编辑器

## 快速开始

```bash
cargo build --release

./target/release/ox          # HTTP/SSE 服务端，默认监听 127.0.0.1:8787
./target/release/ox serve    # HTTP/SSE 服务端，默认监听 127.0.0.1:8787
./target/release/ox acp      # 通过 stdio 提供 ACP agent，供编辑器使用
```

可将 [ox.example.toml](ox.example.toml) 复制为 `~/.ox/ox.toml`，替换 provider
占位值并选择模型。配置文件可缺省启动，但真实模型调用至少需要已配置的 provider
或凭证已解析的内置 provider。

第一个参数决定模式。没有参数，或者第一个参数以 `-` 开头，都会启动 HTTP 服务端。
启动服务端后，可让 OpenCode 连接 `http://127.0.0.1:8787`；如果改了 `listen`，
则连接实际配置的地址。

## 文件位置

| 路径 | 用途 |
|---|---|
| `~/.ox/ox.toml` | 可选配置文件 |
| `~/.ox/ox.db` | 数据库（Turso，兼容 SQLite） |
| `~/.ox/ox.log` | 后台服务端日志 |
| `~/.ox/run/ox.sock` | 私有 unix socket（0600） |
| `~/.ox/run/ox.lock` | 单实例锁 |
| `~/.ox/tmp/` | 临时工作目录；过期条目会按保留策略清理 |

## 配置

`~/.ox/ox.toml` 是可选的：文件不存在时使用默认值，不必为了启动服务端先创建它。
完整示例见 [ox.example.toml](ox.example.toml)。

### 原生 Provider 与模型路由

真实模型调用至少需要已配置的 `[providers.<name>]`，或凭证已解析的内置 provider；
`[models.<id>]` 仅在需要逻辑路由（别名或有序候选链）时才需要；不存在隐式默认
provider。最小示例：

```toml
default_model = "fast"

[providers.example]
base_url = "https://gateway.example.com"
api_key = "replace-with-your-key"
protocol = "chat_completions"

[models.fast]
candidates = [
  { provider = "example", model = "some-model" },
]
```

- `base_url` 填**版本后缀之前**的部分。ox 会按协议追加
  `/v1/chat/completions` 或 `/v1/messages`；Chat Completions 端点不能以 `/v1` 结尾。
- `protocol` 默认为 `chat_completions`，也可设为 `messages`。少数内置 vendor 可以
  省略 `base_url`，由 ox 使用其内置端点和协议。
- 逻辑模型按 `candidates` 顺序尝试，第一个成功打开流的端点服务本轮。传输错误和
  HTTP 401/403/408/429/5xx 会触发下一候选；流打开后不在中途切换。
- 未出现在 `[models.<id>]` 中、未显式指定 provider 且不被内置 provider 拥有的模型
  会以 `NoRoute` 失败，不会静默回退到默认端点；显式指定 provider 的模型和内置
  provider 拥有的模型仍可路由。
- `default_model` 推荐写成 `provider/route-id`（例如 `example/fast`），明确指向路由；
  也可以写模型目录中存在的裸 route id。未知名称不会回退到隐式端点。

### 环境变量

| 变量 | 对应设置与行为 |
|---|---|
| `OX_DB_PATH` | 覆盖 `db_path`；默认 `~/.ox/ox.db`。 |
| `OX_LISTEN` | 覆盖 `listen`；默认 `127.0.0.1:8787`。 |
| `OX_SUBAGENT_DEPTH` | 覆盖 `subagent.depth`；默认 `1`，`0` 禁用子 agent。非法整数会告警并保留配置值。 |
| `OX_DEFAULT_MODEL` | 覆盖 `default_model`；两者都未设置时，依次优先选择已配置的内置 provider 的第一个可聊天模型，再选择模型目录的第一个可聊天条目。 |
| `DEEPSEEK_API_KEY` | 为内置 DeepSeek provider 提供密钥；显式配置的密钥优先。 |

### 配置热重载

服务端通过文件变化监听热重载 `~/.ox/ox.toml`，也可调用
`POST /api/location/reload`。provider、模型路由、MCP 服务器与相关工具注册表都可以
热更新；MCP 配置修改不需要重启服务端。

新配置解析或校验失败、或模型目录刷新失败时，继续使用上一份可运行的配置。内置
provider 的模型列表端点不可达时，仍可使用其内置种子模型。自动文件监听在文件缺失或不可读时会保留当前运行配置；手动重载时若文件缺失，则载入默认配置。

以下键只影响启动时的装配，热重载时会保留运行值，需要重启才能生效：
`listen`、`db_path`、`event_heartbeat_ms`、`[events]`、`[sessions]`、`[tmp]`、
`[turn]`、`[subagent]`、`[magic_context]`。这里的 `[events]` 是事件日志保留配置，
不是 `[mcp.events]`。

### 模型角色

标题生成和上下文压缩可以使用独立逻辑模型。值必须是模型目录认识的逻辑 id：
`[models.<id>]` 的路由键，或单端点模式下的 wire id。

```toml
[roles]
title = "slim"
# compaction = "big"  # 默认跟随会话模型；/slim 那一轮使用该轮模型

[agents.explore]
model = "slim"
```

目录里没有 `slim` 时，标题生成回退到会话模型；`/slim` 只让下一条请求走 `slim`，
不改变会话模型。子 agent 指定的模型不在目录中时会直接报错，而不是静默使用父会话模型。

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

`disabled` 默认为 `false`。设为 `true` 后，启动和配置重载时会跳过该服务器。
手动连接只是临时覆盖；下次成功重载或重启后，重新按配置决定是否连接。

MCP 连接和工具注册表是进程级的，不按会话隔离。连接或断开服务器会影响所有会话；
工具以 `<server>_<tool>` 形式暴露。

### 权限

每次工具调用（内置工具、`ctx_*` 或 MCP）执行前都会检查权限。规则按层叠加
（内置 agent 基础规则 → `[permissions.rules]` → session 规则）；这些规则层内及
跨层都以**最后一条**匹配规则为准，`deny` 立即短路。已保存的授权只在规则要求
询问时参与结算，不覆盖 `allow`/`deny`。
`[permissions] default` 是 `ask` 而不是 `allow`。内置 agent 带有上游基础规则
（`*/*` allow、`read *.env` 与 `read *.env.*` ask、`read *.env.example` allow），
所以普通读取/glob 不会弹窗，读取 `.env` 才会。

```toml
[permissions]
default = "ask"                    # ask | allow | deny

[[permissions.rules]]
action = "shell"
resource = "git push*"
effect = "ask"                     # allow | ask | deny

[[permissions.rules]]             # 永久拒绝；没有 always_deny
action = "edit"
resource = "*.pem"
effect = "deny"

[permissions.tools]                # ox 专有下限，只能收紧结果
shell = { ask_if = ["rm -rf", "git push", "sudo"] }
webfetch = "ask"
```

`action` 和 `resource` 必填，留空会在加载配置时被拒绝。`ask_if` 是匹配调用参数的
正则（`shell` 匹配命令文本，其他工具匹配紧凑 JSON）；命中只能把 `allow` 升级为
`ask`，不能放宽策略。MCP 工具使用带服务器前缀的名称。

`ask` 会发出 `permission.asked` 请求，答复三选：`once`（仅当前调用）、`always`
（把 allow 规则写入 `ox.db` 中项目级的 `permission_saved` 表；删除该记录即撤销，
同一 session/项目的 pending 请求会按新规则重评）和 `reject`（仅当前调用失败，
可附带消息反馈给模型）。没有「本次会话一直允许」选项。子会话的请求使用自己的
session ID 加 `metadata.parentID` 呈现给父会话。300 秒未答复、中断或断线均按拒绝
处理。自动化/unattended 会话永不等待：规则结算后只有 `allow` 放行，其余一律拒绝。

客户端可通过 `/api/config`（permissions 与 agents）、`GET /api/permission/request`、
`GET /api/permission/saved`、`PATCH`/`DELETE /api/permission/saved/{id}`、
`GET /api/session/{id}/permission[/{requestID}]` 和
`POST /api/session/{id}/permission/{requestID}/reply` 读取与管理权限。

## OpenCode 集成

ox 使用 OpenCode 作为 UI。启动 ox HTTP/SSE 服务端后，将 OpenCode 指向兼容的
API endpoint。OpenCode 负责会话交互和展示，ox 负责 agent 执行、会话、工具和
provider 接入。

| 模式 | 命令 | 行为 |
|---|---|---|
| 服务端 | `ox [flags]` 或 `ox serve [--stdout\|--no-stdout] [--log-file PATH] [--service] [--exit-when-idle]` | 运行 HTTP/SSE API 和会话引擎。每个 home 只允许一个服务端：它获取实例锁并绑定私有 unix socket。`--exit-when-idle` 让服务端在没有客户端持有 lease 后退出。 |
| ACP agent | `ox acp [--server URL] [--log-file PATH] [--no-stdout]` | 通过 stdio 使用 ACP v1。不指定 `--server` 时，先尝试本地 unix socket，再尝试配置的 TCP 地址，最后在进程内启动服务端；指定后只连接该地址。日志走 stderr 或 `--log-file`。 |

## ACP 集成

把支持 ACP 的编辑器（Zed、JetBrains 等）的 agent 命令指向 `ox acp`。工作目录由
编辑器按会话给出；ox 把 ACP 会话一对一映射到 ox 会话，把自己的 agent 作为 ACP
模式暴露，并把助手正文、思考内容和工具调用作为 session update 流式推回去。

与普通 CLI 包装不同，`session/load` 会重放已存储的 transcript，包括 magic context
的丢弃内容覆盖层，所以编辑器看到的就是模型当时看到的那份。ox 无法隔离的会话
设置会在创建或改动任何会话之前以 ACP invalid params 错误拒绝，绝不静默丢弃。
MCP 服务器统一配在 `~/.ox/ox.toml` 的 `[mcp.servers.<名字>]` 中，属于进程级配置；
编辑器不能按会话添加。

### ACP v1 稳定能力矩阵

`ox acp` 是通过 stdio 通信的 ACP v1 agent。它的稳定 wire 面有意小于完整 SDK
schema。ox 不启用、不协商任何 `unstable` ACP 能力；SDK 自身传递的
`unstable_proxy_chains` schema feature 不代表 ox 支持该能力。

**已实现**

| 范围 | 稳定能力 |
|---|---|
| 初始化 | `initialize`；返回 ACP v1，声明 `session/load`，并声明会话 list、delete、resume、close 能力。 |
| 会话生命周期 | `session/new`、`session/load`、`session/resume`、`session/list`、`session/delete`、`session/close`。 |
| 会话控制 | `session/prompt`、`session/cancel`、`session/set_config_option`、`session/set_mode`。 |
| 选择 | 模型、模式和思考强度的 `configOptions`；不实现 `session/set_model`。 |
| Prompt 内容 | 文本、图片、文本型 embedded resource。二进制 embedded resource、音频和 resource-link prompt 会被拒绝，不会隐式下载。 |
| 会话更新 | `content/tool/update` 映射：助手正文和思考增量、用户消息重放、工具调用及工具进度/结果、工具输出中的 resource link、仅在上游给出真实 `used`/`size` 时的 `usage_update`，以及按能力门控的 notice/compaction 更新。 |
| 客户端交互 | permission 请求会桥接给客户端。只有客户端声明稳定的 form elicitation 能力时才使用 elicitation；否则取消表单并 fail-closed。 |

**Fail-closed（ox 无法兑现的请求设置）**

| 请求 | 行为 |
|---|---|
| `session/new` / `session/load` / `session/resume` 上的 `systemPrompt`、`additionalDirectories` | 在任何 HTTP session 操作之前以 invalid params 拒绝，绝不静默丢弃。 |
| 同样三个方法上的按会话 `mcpServers` | 在创建、加载或恢复任何 session 之前以 invalid params 拒绝：ox 无法保证工具注册表按会话隔离。 |
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

## Automations 与 MCP Events

Automations 和 MCP Events 都默认关闭。分别启用：

```toml
automations = true

[mcp.events]
enabled = true
poll_interval = 5000 # 毫秒；必须为正数
```

Automations 支持四类触发器：

- `at`：在指定时间触发一次。
- `every`：按固定间隔重复触发。
- `cron`：按 cron 表达式触发，必须提供显式或默认 IANA 时区。
- `event`：订阅已连接 MCP 服务器的命名事件。

Action 可以是发送给现有 session 或指定 subagent 的 `prompt`、持久化执行的
`shell` 命令，以及在受限 QuickJS 环境中运行的 `code`。未获得授权的无人值守 action
会失败，不会在运行时自动征求交互授权。

管理入口包括 `/automation`、`automation.*` 工具和
`/api/experimental/automation` HTTP API。HTTP API 支持列出、查看、创建、暂停、恢复和
删除；shell/code action 与 event 触发器需要本地交互会话授予的权限，应通过工具创建。

MCP Events 实现 MCP Events beta 扩展。服务器必须声明该扩展，ox 才会发起订阅；
对每种事件类型，按服务器公布的投递能力依次优先选择 **push**、**poll**、
**webhook**：

- `push`：通过事件流接收并维护 cursor。
- `poll`：按 `poll_interval` 轮询事件日志，间隔不会短于该值。
- `webhook`：接收 HMAC-SHA256 签名的回调；需要在 `[mcp.events.webhook]` 配置
  `public_url` 和 `secret`。

缺少任一 webhook 配置或签名无效时会 fail-closed，不会发布为 MCP 事件。
外部 MCP 事件 payload 会一直标记为不可信数据，不能作为可信本地指令进入事件总线。

## 内置 agent

| Agent | 模式 | 职责与工具范围 |
|---|---|---|
| `apex` | Primary，默认 | 面向用户的默认 agent，可使用完整工具注册表。 |
| `general` | Subagent，默认 | 通用子 agent，使用除 `question` 外的已注册工具；默认继承父会话模型。 |
| `explore` | Subagent | 只读代码库和网页探索，可使用上下文检索；没有 shell、结构化写入工具、子 agent 或 MCP 工具；优先使用 `slim`，也是 agent 无法读取时的 fail-closed 回退。 |
| `review` | Subagent | 评审不是自己完成的工作；使用读取/检索工具和为 git、验证保留的 `shell`，不含结构化写入工具。 |
| `rust-expert` | Subagent | 使用 Rust 专用评审提示层和同一套评审工具，进行严格 Rust 代码审查。 |

评审 agent 的 `shell` 用于查看 diff 和运行验证，不是沙箱边界；真正需要隔离的代码
仍应放在沙箱中执行。子会话模型可通过 `[agents.<id>]` 覆盖；primary agent 使用其
持久化的会话模型。

## 仓库结构

| Crate | 职责 |
|---|---|
| `ox`（根） | `ox` 二进制，以及对外暴露已装配模块的 library façade |
| `ox-schema` | opencode v2 协议的 serde 模型与共享路径 |
| `ox-db` | Turso 连接、PRAGMA、版本化 schema、单写者闸门与持久化存储 |
| `ox-llm` | 端点、凭据、模型目录、原生路由、两套聊天协议和 SSE 分帧 |
| `ox-core` | 配置、凭据存储、事件总线及其 projector |
| `ox-engine` | 按会话的 runner、magic context 和系统提示词 |
| `ox-tools` | 内置工具集：文件、shell 和网页工具，以及子会话、todo、skill、automation 与上下文工具 |
| `ox-codemode` | Code Mode 的静态扫描、工具路径判定和资源受限 QuickJS 执行器 |
| `ox-automation` | Automation 领域模型、持久化存储、租约、调度器和 action 执行器 |
| `ox-mcp` | MCP 客户端 hub、工具桥接与 Events wire 层 |
| `ox-server` | HTTP API、ACP 后端装配、配置重载、实例锁与生命周期 |
| `ox-acp` | 通过公开 HTTP 契约驱动 ox 的 ACP stdio 翻译层 |

依赖方向保持单向。`ox-schema` 是共享契约；`ox-llm` 只依赖它，不依赖其它 ox
crate；`ox-core` 在其上加入配置、存储相关能力和事件总线；引擎与工具集位于更高层，
由 `ox-server` 装配。`ox-acp` 是客户端，只使用公开 HTTP 契约，不直接访问 runner。

## 开发

提交前依次运行以下闸门：

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release
```

仓库包含自动化测试；修改行为时应同时补充或更新覆盖该行为的测试。

## 文档

- [ox.example.toml](ox.example.toml) —— 带英文注释的完整配置示例
- [skills/README.md](skills/README.md) —— 项目 skill 的加载与软链接方式
- [tools/local-eval/README.md](tools/local-eval/README.md) —— 可审计的本地 ACP runner 与冒烟测试
- [ACP v1 稳定能力矩阵](#acp-v1-稳定能力矩阵) —— ACP 的稳定接口、fail-closed 行为和排除项
- [内置 agent 定义](crates/ox-tools/src/agent.rs) —— agent、工具范围和模型偏好的权威来源
- [ACP 入口](crates/ox-acp/src/lib.rs) —— stdio ACP v1 服务与请求分发
