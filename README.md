# ox

**English** · [简体中文](README.zh-CN.md)

## Overview

ox is an independent, pure-Rust **opencode v2 compatible agent runtime**. It
implements the API OpenCode uses rather than reusing the Bun implementation:
OpenCode provides the user-facing conversation, while ox owns the agent
runtime, sessions, tools and provider access.

One binary provides both:

- an **HTTP/SSE server** for OpenCode-compatible clients, listening on
  `127.0.0.1:8787` by default; and
- an **ACP agent** over stdio for editor integrations.

ox keeps its database and configuration under `~/.ox`; it does not read or write
the Bun implementation's files.

## Features

- **Native provider routing:** `[providers.<name>]` defines endpoints and
  `[models.<id>]` defines ordered candidates. Transport and retryable HTTP
  failures can fail over before a stream opens; an established stream is never
  retried mid-turn.
- **MCP:** process-wide local stdio and remote MCP servers expose resources and
  namespaced tools.
- **Automations and MCP Events:** scheduled, cron and MCP-event triggers can run
  prompt, Code Mode or shell actions. MCP Events supports push, poll and signed
  webhook delivery.
- **Magic context:** durable project memories, searchable notes, transcript
  compartments, context folding and background consolidation through the
  `ctx_*` tools.
- **Built-in agents:** `apex`, `general`, `explore`, `review` and `rust-expert`,
  with explicit tool boundaries and child-session model preferences.
- **Editor integration:** ACP session creation, replay, listing, cancellation
  and tool updates, with unsupported setup rejected fail-closed.
- **Safety controls:** per-tool allow/ask/deny policies and invalid-config reloads
  that preserve the running generation.

## Requirements

- A Rust toolchain with edition 2024 support (Rust 1.85 or newer) to build.
- macOS or Linux. The server binds a private Unix socket, so Windows is not
  supported.
- A configured model provider for real model turns.
- OpenCode, if you want to use it as the UI; an ACP-capable editor, if you want
  editor integration.

## Quick start

```bash
cargo build --release

./target/release/ox          # HTTP/SSE server on 127.0.0.1:8787 by default
./target/release/ox serve    # explicit server mode, same default
./target/release/ox acp      # ACP v1 agent over stdio
```

Copy [ox.example.toml](ox.example.toml) to `~/.ox/ox.toml`, replace the provider
placeholders and select a model before starting the server. The config file is
optional for startup; real model turns need at least one configured provider or
a built-in provider whose credential has resolved.

The first argument selects the mode. No argument, or an argument beginning with
`-`, starts the HTTP server.

## Files

| Path | Purpose |
|---|---|
| `~/.ox/ox.toml` | Optional TOML configuration |
| `~/.ox/ox.db` | Turso database (SQLite-compatible) |
| `~/.ox/ox.log` | Log file for a detached server |
| `~/.ox/run/ox.sock` | Private Unix socket (`0600`) |
| `~/.ox/run/ox.lock` | Single-instance lock |
| `~/.ox/tmp/` | Scratch files and other temporary working data |

## Configuration

The complete commented reference is [ox.example.toml](ox.example.toml).
`~/.ox/ox.toml` is optional and sections have defaults where applicable, but
there is no implicit provider. Top-level keys such as `listen`, `db_path` and
`default_model` sit beside `[providers]`, `[models]`,
`[context_caps]`, `[roles]`, `[agents]`, `[magic_context]`, `[mcp]`,
`[permissions]`, `[subagent]`, `[events]`, `[sessions]`, `[tmp]` and `[turn]`.

### Provider and model routing

Real model turns need at least one configured `[providers.<name>]` or a built-in
provider with a resolved credential. `[models.<id>]` is only needed for logical
routes (aliases and ordered candidate chains); there is no implicit default
provider. Native routing starts with:

```toml
default_model = "fast"

[providers.primary]
base_url = "https://gateway.example.com"
api_key = "REPLACE_WITH_KEY"
protocol = "chat_completions" # or "messages"

[providers.backup]
base_url = "https://backup.example.com"
api_key = "REPLACE_WITH_BACKUP_KEY"
protocol = "chat_completions"

[models.fast]
candidates = [
  { provider = "primary", model = "some-model" },
  { provider = "backup", model = "some-model" },
]
```

`base_url` is the part **before** the protocol's version suffix. ox appends
`/v1/chat/completions` for Chat Completions and `/v1/messages` for the Anthropic
Messages protocol. Candidates are tried in order; the first endpoint whose
stream opens serves the turn. Model ids in a candidate are the wire ids reported
by that endpoint. A single-endpoint model may instead use `provider/id`.

`default_model` should be written as `provider/route-id` (for example
`primary/fast`) to name a route explicitly, or as a bare route id that exists in
the model catalogue. An unknown name fails closed instead of selecting an
arbitrary endpoint.

A small built-in table supplies endpoints and protocols for known vendors, so a
section such as `[providers.deepseek]` may state only `api_key`. Built-in
providers appear only after a key resolves.

### Model roles

Auxiliary requests can use their own logical models:

```toml
[roles]
title = "slim"        # default: the logical model `slim`
# compaction = "big"  # default: the session's model (or a /slim turn's model)

[agents.explore]
model = "slim"        # child-session preference; default comes from the agent table
```

Title generation falls back to the session model when `slim` is missing. A
`/slim` prompt runs one turn on `slim` without changing the session's selected
model. A subagent whose configured model is absent from the catalogue refuses
to start instead of silently using its parent's model.

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

`disabled` defaults to `false`. Disabled servers are skipped at startup and on
reload. A manual connection is a temporary override; the next successful reload
or server restart reapplies the configured value.

MCP connections and the tool registry are process-wide, not session-specific.
Connecting or disconnecting a server affects every session. Tools are exposed
under their server name as a prefix. MCP configuration changes are hot-reloaded;
they do not require a restart.

### Permissions

Every tool call — built-in, `ctx_*` or MCP — is checked before it runs. Rules
are grouped by layers (built-in agent defaults, `[permissions.rules]`, session
rules); within and across those layers the **last** matching rule wins, and a
`deny` short-circuits. Saved approvals are consulted only when a rule asks —
they never override an `allow` or `deny`. `[permissions] default` is `ask`, not
`allow`. Built-in agents seed upstream's base rules (`*/*` allow, `read *.env`
and `read *.env.*` ask, `read *.env.example` allow), so ordinary reads/globs
run without a prompt while `.env` files ask.

```toml
[permissions]
default = "ask"                    # ask | allow | deny

[[permissions.rules]]
action = "shell"
resource = "git push*"
effect = "ask"                     # allow | ask | deny

[[permissions.rules]]             # permanent refusal; there is no always_deny
action = "edit"
resource = "*.pem"
effect = "deny"

[permissions.tools]                # ox-specific floor; only tightens results
shell = { ask_if = ["rm -rf", "git push", "sudo"] }
webfetch = "ask"
```

`action` and `resource` are required: a blank value is rejected when the
config is loaded. `ask_if` holds regular expressions matched against the call
arguments (command text for `shell`, compact JSON otherwise); a match can only
escalate `allow` to `ask`, never relax a policy. MCP tools use their prefixed
names.

An `ask` opens a `permission.asked` request. The three replies are `once`
(this call only), `always` (persist an allow rule in the project-scoped
`permission_saved` table in `ox.db`; delete the saved row to revoke it, and
same-session/same-project pending requests are re-evaluated against the new
rule) and `reject` (fail only this call; an optional message is returned to
the model). Session-scoped grants are not offered. A child session's
request uses its own session ID plus `metadata.parentID` so the parent can
display it. No answer within 300 seconds, an interrupt or a disconnect counts
as a rejection. Automation/unattended sessions never wait: rules are settled
and only `allow` proceeds; anything else is denied.

Clients can read and manage permissions through `/api/config` (permissions and
agents), `GET /api/permission/request`, `GET /api/permission/saved`,
`PATCH`/`DELETE /api/permission/saved/{id}`,
`GET /api/session/{id}/permission[/{requestID}]` and
`POST /api/session/{id}/permission/{requestID}/reply`.

### Environment variables

| Variable | Effect |
|---|---|
| `OX_DB_PATH` | Overrides the database file path. |
| `OX_LISTEN` | Overrides the HTTP server listen address. |
| `OX_SUBAGENT_DEPTH` | Overrides `[subagent].depth`; invalid values are ignored with a warning. |
| `OX_DEFAULT_MODEL` | Overrides `default_model`. |
| `DEEPSEEK_API_KEY` | Supplies the built-in DeepSeek provider key when no configured or stored key is present. |

### Hot reload and restart-only keys

ox watches the config file's directory and reloads when the config content
changes; it does not poll. A valid edit rebuilds hot-swappable services,
including providers, model routes, model roles, permissions, agents and MCP.

An invalid TOML document or a reload that cannot initialize its services leaves
the previous running configuration in place. The automatic file watcher also
keeps the current configuration when the file is missing or unreadable. A manual
`POST /api/location/reload` with a missing config file loads the default
configuration.

The following values are pinned to their running generation and require a
server restart to change:

- `listen`
- `db_path`
- `event_heartbeat_ms`
- `[events]`
- `[sessions]`
- `[tmp]`
- `[turn]`
- `[subagent]`
- `[magic_context]`

## OpenCode integration

Start the ox server, then point your OpenCode-compatible client at
`http://127.0.0.1:8787`. OpenCode owns conversation presentation; ox serves the
opencode v2 HTTP/SSE API and owns execution, persistence, tools and provider
routing.

| Mode | Command | Behaviour |
|---|---|---|
| Server | `ox [flags]` or `ox serve [--stdout\|--no-stdout] [--log-file PATH] [--service] [--exit-when-idle]` | Runs the HTTP/SSE API and session engine. One server per home takes the instance lock and binds the private Unix socket. `--exit-when-idle` exits once no client holds a lease. |
| ACP agent | `ox acp [--server URL] [--log-file PATH] [--no-stdout]` | Speaks ACP v1 over stdio. Without `--server`, tries the local Unix socket, then the configured TCP address, then starts a server in-process. With `--server`, only that endpoint is used. Logs go to stderr or `--log-file`. |

## ACP integration

Point an ACP-capable editor (Zed, JetBrains, …) at `ox acp` as its agent command.
The editor supplies the working directory per session; ox maps ACP sessions
one-to-one onto ox sessions, exposes its agents as ACP modes, and streams
assistant text, reasoning and tool calls back as session updates.

Unlike a plain CLI shim, `session/load` replays the stored transcript, including
the magic-context dropped-content overlay, so the editor shows what the model
actually saw. MCP servers are configured process-wide under
`[mcp.servers.<name>]`; an editor cannot add one per session.

### ACP v1 stable capability matrix

`ox acp` is a stdio ACP v1 agent. Its stable wire surface is deliberately
smaller than the full SDK schema. No `unstable` ACP feature is enabled or
negotiated; the SDK's own transitive `unstable_proxy_chains` schema feature is
not an ox capability.

**Implemented**

| Area | Stable surface |
|---|---|
| Initialization | `initialize`; replies with ACP v1, advertises `session/load` and session listing, deletion, resumption and close capabilities. |
| Session lifecycle | `session/new`, `session/load`, `session/resume`, `session/list`, `session/delete`, and `session/close`. |
| Session control | `session/prompt`, `session/cancel`, `session/set_config_option`, and `session/set_mode`. |
| Selection | `configOptions` for model, mode and thought level; `session/set_model` is not implemented. |
| Prompt content | Text, images, and text embedded resources. Binary embedded resources, audio, and resource-link prompts are rejected rather than fetched implicitly. |
| Session updates | `content/tool/update` mappings for assistant text and thought deltas, user-message replay, tool calls and tool progress/results, resource links in tool output, `usage_update` only when the upstream event supplies a real `used`/`size`, and capability-gated notice/compaction updates. |
| Client interactions | Permission requests are bridged to the client. Elicitation is used only when the client advertises the stable form-elicitation capability; otherwise the form is cancelled and the request fails closed. |

**Fail-closed (requested setup ox cannot honour)**

| Request | Behaviour |
|---|---|
| `systemPrompt`, `additionalDirectories` on `session/new` / `session/load` / `session/resume` | Rejected with invalid params before any HTTP session work; never silently dropped. |
| Per-session `mcpServers` on those same three methods | Rejected with invalid params before any session is created, loaded or resumed: ox cannot prove session-local tool isolation. |
| Elicitation when the client did not advertise form elicitation | Pending form cancelled and request fails closed; never auto-approved. |
| Client filesystem (`fs`) / terminal callbacks | Not advertised and never issued by ox. |

**Explicitly excluded**

- ACP v2 and all `unstable` ACP capability families (proxy chains, fork,
  archive/unarchive, set_title, status, …).
- MCP-over-ACP and HTTP/SSE MCP; `mcpCapabilities.http`/`sse`/`acp` are not
  advertised. MCP configured in `~/.ox/ox.toml` is process-wide only.
- HTTP/SSE ACP transports: the HTTP/SSE server behind `ox acp` is an
  implementation detail, not an ACP transport.
- provider/connect.
- Binary embedded resources, audio and resource-link prompts are rejected,
  not fetched implicitly.

## Automations and MCP Events

Both features are **disabled by default**. Enable automations with the top-level
`automations` key. Use `timezone` for a default IANA zone for wall-clock
triggers:

```toml
automations = true
timezone = "UTC" # optional; UTC is used when no zone is configured
```

An automation combines one trigger with one action:

- **Triggers:** `at` for a one-shot time, `every` for an interval, `cron` for a
  cron expression, or `event` for an event from a connected MCP server. Cron
  triggers require a timezone.
- **Actions:** `prompt` for a model turn, `code` for a resource-limited Code Mode
  JavaScript program, or `shell` for a command.

Manage automations through the OpenCode `/automation` command, the HTTP API at
`/api/experimental/automation` (list, create, get, delete, pause and resume), or
the `automation.create`, `automation.list`, `automation.pause`,
`automation.resume` and `automation.delete` tools. The HTTP create path accepts
scheduled prompt actions; event triggers and unattended shell/code actions must
be created through a tool call in a local session so ox can obtain an interactive
permission grant. Unsupported or unauthorized setup fails closed.

MCP Events bridges event types exposed by connected MCP servers into ox's event
bus, where they can drive automation event triggers. Enable it separately:

```toml
[mcp.events]
enabled = true
poll_interval = 5000 # milliseconds; must be positive
```

For each advertised event type, ox uses the first delivery mode offered by the
server in this order: **push**, **poll**, then **webhook**. Polling re-reads the
event log no more often than `poll_interval`. Webhook subscriptions additionally
require a public callback URL and signing secret:

```toml
[mcp.events.webhook]
public_url = "https://example.com/ox/mcp-events"
secret = "REPLACE_WITH_A_LONG_RANDOM_SECRET"
```

Webhook subscriptions fail closed without both values. Incoming webhook bodies
must carry a valid signature; rejected deliveries are not published as MCP
events.

## Built-in agents

| Agent | Mode | Responsibility and tool boundary |
|---|---|---|
| `apex` | Primary, default | General implementation and orchestration with the full registered toolset. Uses the session's persisted model. |
| `general` | Subagent, default | General delegated work with all registered tools except `question`; inherits the parent model unless configured otherwise. |
| `explore` | Subagent | Read-only codebase and web exploration with context search; no shell, structured writers, subagents or MCP tools. Prefers `slim`. |
| `review` | Subagent | Reviews work it did not perform using read/search tools plus shell for verification. Structured writers, context writers, subagents and MCP tools are absent. |
| `rust-expert` | Subagent | Applies a strict Rust review lens with the same review toolset as `review`. |

The review toolset is read-constrained, not sandboxed: shell can execute build
scripts or tests from the tree under review. A hard read-only boundary requires
a sandbox. Model preferences under `[agents.<id>]` apply only to child sessions;
primary sessions use their persisted model.

## Repository layout

| Crate | Responsibility |
|---|---|
| `ox` (root) | The `ox` binary and an integration-test façade re-exporting the assembled runtime. |
| `ox-schema` | Serde models for the opencode v2 wire protocol and shared paths. |
| `ox-db` | Turso storage, pragmas, versioned schema and the single-writer gate. |
| `ox-llm` | Provider configuration, endpoints, credentials, model catalogue, Chat Completions/Messages clients and SSE framing. |
| `ox-core` | Configuration, credential storage, the event bus and its projector. |
| `ox-engine` | Per-session runner, magic context and system-prompt composition. |
| `ox-tools` | Built-in file, shell and web tools plus subagents, todos, skills, automation and context tools. |
| `ox-mcp` | MCP client hub and MCP Events protocol client. |
| `ox-server` | HTTP API, hot reload, automation scheduling, ACP endpoint discovery, instance lock and lifecycle. |
| `ox-acp` | ACP v1 translator for editors. |
| `ox-automation` | Automation triggers/actions, cron and due-time decisions, durable storage, leasing and scheduler. |
| `ox-codemode` | Static JavaScript/tool scanning and a resource-limited QuickJS execution runtime. |

## Development

Run all four gates from the repository root:

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release
```

The workspace has automated unit and integration tests. `cargo test` covers
configuration, routing, protocol behavior and runtime services; formatting,
tests, warning-free clippy and the release build are all required before
submitting changes.

## Docs

- [Skills](skills/README.md) — project skill layout and loading.
- [Local evaluation](tools/local-eval/README.md) — auditable ACP runner and
  smoke harness.
- [简体中文](README.zh-CN.md) — Chinese project guide.
