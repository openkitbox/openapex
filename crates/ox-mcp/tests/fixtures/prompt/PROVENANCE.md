# Prompt golden fixtures — provenance

- Upstream anchor: `/Users/john/github/opencode` (`reference/agents/opencode`)
- Upstream commit: `8482dbf1e7eba18d6c5d0de6f02f8242112bcf06` (v2.1.2 tree)
- Captured: 2026-10-23, batch B of `.ox/plans/ox-injection-hardening.md`

| Fixture | Upstream source | Kind |
|---|---|---|
| `mcp-block.txt` | `packages/core/src/mcp/instructions.ts:17-29` (`entries`/`render`: server name in the attribute, every instruction line indented 4 spaces) | hand-written expected output |

## Substitution rules applied by the tests (nothing else may differ)

None: the fixture is compared byte-for-byte against `Hub::instructions_block()`
for a fixed one-server input (`stub`, two instruction lines).

Pre-existing upstream divergence frozen deliberately: upstream adds a
`Use tools from this server through \`execute\` under \`tools[...]\`.` line
per server when codemode is on; ox omits it (batch I of the plan adds it and
will update this fixture in the same commit). ox XML-escapes the server name;
the fixed input has no escapable characters.

## Batch I deviation (2026-10-23, appended — everything above stays as written)

`mcp-block.txt` is unchanged (checksum below still verifies). The G4 test no
longer compares the whole block byte-for-byte: the upstream bytes are a strict
**prefix**, and exactly one ox-specific line follows:

> Use tools from these servers directly by their `<server>_<tool>` names
> (sanitized server name, underscore, sanitized tool name).

Why a line instead of the upstream line: upstream's per-server line
(`instructions.ts:22`) names the codemode bridge — "Use tools from this server
through \`execute\` under \`tools[\"<server>\"]\`" — which does not exist in ox.
ox registers MCP tools directly under their qualified names
(`ox_mcp::tool::qualified_name`: sanitized server, `_`, sanitized tool), so the
naming guidance is adapted to that mechanism and appended once after the block
instead of per server. Wire effect after deploy: every session's frozen
`core/mcp-guidance` source changes for servers that supply instructions → one
keyed delta on the first step (documented keyed-framework re-baseline).

## Checksums (verified at test time; keep in sync when a fixture changes)

```
7667f6194bf26f256d9464c2aedf7e0afb69b0e984d2162c5022397d210bf58e  mcp-block.txt
```
