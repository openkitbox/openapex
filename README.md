# ox

**English** · [简体中文](README.zh-CN.md)

OpenApex is an independent agent implementation for the opencode v2 chat
protocol. It uses OpenCode as its UI: OpenCode handles the user-facing
conversation, while OpenApex provides the agent runtime, session engine, tools,
and provider integration. One binary provides the HTTP/SSE **server** and an
**ACP agent** for editor integrations.

ox keeps its own database and configuration under `~/.ox`; it never reads or
writes the Bun implementation's files.

## Requirements

- Rust with edition 2024 support (1.85 or newer)
- macOS or Linux (the server binds a unix socket, so Windows is not supported)

## Quick start

```bash
cargo build --release

./target/release/ox          # HTTP server on 127.0.0.1:8787 by default
./target/release/ox serve    # HTTP server on 127.0.0.1:8787 by default
./target/release/ox acp      # ACP agent over stdio, for editors
```

## OpenCode integration

OpenApex is designed to use OpenCode as its UI. Run the OpenApex HTTP server,
then point OpenCode at its compatible API endpoint; OpenCode owns the
conversation and presentation while OpenApex owns agent execution, sessions,
tools and provider access.

The first argument picks the mode. Anything starting with `-`, or no argument
at all, starts the HTTP server.

| Mode | Command | Behaviour |
|---|---|---|
| Server | `ox [flags]` or `ox serve [--stdout\|--no-stdout] [--log-file PATH] [--exit-when-idle]` | Runs the HTTP/SSE API and the session engine. One server per home: it takes the instance lock and binds the private unix socket. `--exit-when-idle` makes it exit once no client holds a lease. |
| ACP agent | `ox acp [--server URL] [--log-file PATH] [--no-stdout]` | Speaks the Agent Client Protocol on stdin/stdout as newline-delimited JSON-RPC. Attaches to a running server when `GET /api/info` answers, otherwise hosts the server in-process on an ephemeral loopback port. stdout is reserved for the wire, so logs go to stderr or `--log-file`. |

## Files

| Path | Purpose |
|---|---|
| `~/.ox/ox.toml` | Configuration |
| `~/.ox/ox.db` | Database (turso, SQLite-compatible) |
| `~/.ox/ox.log` | Log of a detached server |
| `~/.ox/run/ox.sock` | Private unix socket (0600) |
| `~/.ox/run/ox.lock` | Single-instance lock |

## Configuration

`~/.ox/ox.toml` is optional; every section has a default. `OX_DB_PATH`,
`OX_LISTEN` and `OX_SUBAGENT_DEPTH` override the matching settings.

Top-level keys such as `listen`, `db_path` and `default_model` sit next to the
sections `[providers]`, `[models]`, `[context_caps]`, `[roles]`, `[agents]`,
`[magic_context]`, `[mcp]`, `[permissions]`, `[subagent]`, `[events]`,
`[sessions]`, `[tmp]` and `[turn]`.

### Provider

A provider is an outbound endpoint plus a key. `base_url` is the part **before**
the version suffix — ox appends the protocol's path (`/v1/chat/completions` for
Chat Completions, `/v1/messages` for the Anthropic Messages protocol):

```toml
default_model = "my-gateway/some-model"

[providers.my-gateway]
base_url = "https://gateway.example.com"
api_key = "…"
# protocol = "chat_completions"   # or "messages"; defaults to chat_completions
```

Model ids are whatever the endpoint reports from its model list, qualified as
`provider/id`. A small built-in table supplies an endpoint and protocol for a
few vendors, so a `[providers.<vendor>]` section may state only `api_key`.

`[models.<id>] candidates` turns one logical model into an ordered failover
list; the first endpoint whose stream opens serves the turn:

```toml
[models.fast]
candidates = [
  { provider = "my-gateway", model = "some-model" },
  { provider = "backup", model = "some-model" },
]
```

The older `cardinal_url` / `cardinal_key` keys still describe the implicit
default endpoint, which routes may reference by its provider name.

### Model roles

Three requests are not the session's own turn, so each can name its own logical
model. Values are logical ids the model catalogue knows (`[models.<id>]` route
keys, or the wire id in single-endpoint mode):

```toml
[roles]
title = "slim"        # default: the logical model `slim`
# compaction = "big"  # default: the session's model (or a /slim turn's)

[agents.explore]
model = "slim"        # default comes from the built-in agent table
```

Title generation falls back to the session model when `slim` is missing, and a
`/slim` prompt asks for one turn on `slim` without changing the session. A
subagent whose agent names a model refuses to start when that model is not in
the catalogue, instead of silently running on the parent's model.

### MCP

Configure local and remote MCP servers in `~/.ox/ox.toml`:

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

`disabled` defaults to `false`. A disabled server is skipped at startup and on
configuration reload. A manual connection is a temporary override; the next
successful reload or server restart applies the configured value again. After
editing the configuration, restart the server to apply it.

MCP connections and the tool registry are process-wide, not session-specific.
Connecting or disconnecting a server affects every session. Its tools are exposed
under the server name as a prefix.

### Permissions

Every tool call passes a policy check before it runs. The default is
`allow`; `[permissions]` tightens it per tool:

```toml
[permissions]
default = "allow"                 # allow | ask | deny

[permissions.tools]
shell = { ask_if = ["rm -rf", "git push", "sudo"] }   # ask only on a match
webfetch = "ask"                  # ask every time
write = "deny"                    # refuse outright
```

`ask_if` holds regexes matched against the call's arguments (the `command`
for `shell`, compact JSON otherwise). An `ask` opens a question on the
user's root session — subagent calls surface there too — with the answers
*once*, *for this session* and *deny*; no answer within five minutes, or an
interrupt, counts as a denial. Session grants live in process memory and
reset on restart. MCP tools are addressed by their prefixed name.

## Editor integration

Point an ACP-capable editor (Zed, JetBrains, …) at `ox acp` as its agent
command. The editor supplies the working directory per session; ox maps ACP
sessions one-to-one onto ox sessions, exposes its agents as ACP modes, and
streams assistant text, reasoning and tool calls back as session updates.

Two things differ from a plain CLI shim:

- MCP servers requested per ACP session are ignored with a warning — ox's tool
  registry is process-global, so MCP servers come from `[mcp.servers.<name>]` in
  `~/.ox/ox.toml` instead.
- `session/load` replays the stored transcript, including the magic-context
  dropped-content overlay, so the editor shows what the model actually saw.

## Repository layout

| Crate | Responsibility |
|---|---|
| `ox` (root) | The `ox` binary and a façade re-exporting every crate |
| `ox-schema` | Serde models for the opencode v2 wire protocol and shared paths |
| `ox-db` | Storage: turso connection, pragmas, versioned schema, single-writer gate |
| `ox-llm` | Endpoints, credentials, model catalogue, the two chat protocols and SSE framing |
| `ox-core` | Configuration, the event bus and its projector |
| `ox-engine` | Per-session runner, magic context and the system prompt |
| `ox-tools` | Built-in toolset: file and shell tools ported from upstream, plus subagents, todos, skills and search |
| `ox-mcp` | MCP client hub |
| `ox-server` | HTTP API, instance lock and lifecycle |
| `ox-acp` | ACP translator for editors |

Dependencies point one way. `ox-schema` is the contract everything shares;
`ox-llm` depends on it and nothing else of ox's; `ox-core` adds storage and the
event bus; the engine and the toolset sit above that; `ox-server` composes them.
`ox-acp` is a client — it speaks the public HTTP contract rather than reaching
into the runner.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo build --release
```

`cargo clippy --workspace --all-targets` is the quality gate: the tree carries
no automated test suite by design, so treat a clean clippy run as the bar.

## Docs

- [Skills](skills/README.md) — loading project skills
