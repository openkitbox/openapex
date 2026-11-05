# ox

**English** · [简体中文](README.zh-CN.md)

A pure-Rust implementation of the opencode v2 chat protocol. One binary, three
modes: an HTTP/SSE **server**, a terminal **TUI**, and an **ACP agent** that
lets editors drive ox directly.

ox keeps its own database and configuration under `~/.ox`; it never reads or
writes the Bun implementation's files.

## Requirements

- Rust with edition 2024 support (1.85 or newer)
- macOS or Linux (the server binds a unix socket, so Windows is not supported)

## Quick start

```bash
cargo build --release

./target/release/ox          # TUI: attaches to a running server, starting one if needed
./target/release/ox serve    # HTTP server on 127.0.0.1:8787 by default
./target/release/ox acp      # ACP agent over stdio, for editors
```

The TUI needs a provider to talk to before it can answer a prompt — see
[Configuration](#configuration).

## Modes

The first argument picks the mode. Anything starting with `-`, or no argument
at all, opens the TUI.

| Mode | Command | Behaviour |
|---|---|---|
| TUI | `ox [--server URL] [--directory DIR] [--session ID]` | Attaches to a server over `~/.ox/run/ox.sock`, spawning `ox serve --exit-when-idle` when nothing is listening and holding a lease so it exits with the last client. `--server` switches to plain HTTP with no spawn and no lease. |
| Server | `ox serve [--stdout\|--no-stdout] [--log-file PATH] [--exit-when-idle]` | Runs the HTTP/SSE API and the session engine. One server per home: it takes the instance lock and binds the private unix socket. `--exit-when-idle` makes it exit once no client holds a lease. |
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
sections `[providers]`, `[models]`, `[context_caps]`, `[magic_context]`,
`[mcp]`, `[subagent]`, `[events]`, `[sessions]`, `[tmp]` and `[turn]`.

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

### MCP

`[mcp.servers.<name>]` connects an MCP server at startup and exposes its tools
under the server name as a prefix. Connections are process-wide, so the tool
registry is the same for every session.

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
| `ox-tui` | Terminal client — see [its README](crates/ox-tui/README.md) |
| `ox-acp` | ACP translator for editors |

Dependencies point one way. `ox-schema` is the contract everything shares;
`ox-llm` depends on it and nothing else of ox's; `ox-core` adds storage and the
event bus; the engine and the toolset sit above that; `ox-server` composes them.
`ox-tui` and `ox-acp` are clients — they speak the public HTTP contract rather
than reaching into the runner.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo build --release
```

`cargo clippy --workspace --all-targets` is the quality gate: the tree carries
no automated test suite by design, so treat a clean clippy run as the bar.

A `Taskfile.yml` wraps the common flows: `task clippy` (fmt plus clippy),
`task prod-build` (release build with `-C target-cpu=native`), `task prod`
(that build, installed to `/opt/bin`) and `task tui` (release TUI pointed at a
running server).

## Docs

- [TUI](crates/ox-tui/README.md) — keys, layout, demo
- [Skills](skills/README.md) — loading project skills
