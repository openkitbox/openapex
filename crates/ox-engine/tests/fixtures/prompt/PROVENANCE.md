# Prompt golden fixtures — provenance

- Upstream anchor: `/Users/john/github/opencode` (`reference/agents/opencode`)
- Upstream commit: `8482dbf1e7eba18d6c5d0de6f02f8242112bcf06` (v2.1.2 tree)
- Captured: 2026-10-23, batch B of `.opencode/plans/ox-injection-hardening.md`

| Fixture | Upstream source | Kind |
|---|---|---|
| `system.txt` | `packages/core/src/session/runner/prompt/system.txt` | byte copy (verbatim, incl. trailing newline) |
| `skills.txt` | `packages/core/src/skill/instructions.ts:25-33` (`render`) | hand-written expected output |
| `instruction-files.txt` | `packages/core/src/session/instructions.ts:76` (``Instructions from: ${path}\n${content}`` joined by `\n\n`) | hand-written expected output |
| `date.txt` | `packages/core/src/instructions/builtins.ts:29` (`Today's date: ${date}`) | hand-written expected output |
| `env.txt` | `packages/core/src/instructions/builtins.ts:36-53` (`<env>` block + initial wrapper) | hand-written expected output with tokens |

## Substitution rules applied by the tests (nothing else may differ)

1. `system.txt`: replace the identity clause `OpenCode, a coding agent harness`
   with `ox, an opencode-compatible server` (the ONLY sanctioned difference),
   then replace `${OPENCODE_TOOL_GUIDANCE}` with the upstream bullets of
   `packages/core/src/session/system-prompt.ts:11-32` for tools
   `shell`/`write`/`edit` (the bullets are quoted verbatim in the test).
2. `skills.txt`: ox deviation — a skill without a description renders no
   `<description>` line (upstream assumes one is always present). `beta`
   exercises that branch. ox also XML-escapes id/name/description; the fixed
   input contains no escapable characters.
3. `env.txt`: `@PLATFORM@` → `std::env::consts::OS`, `@OX_TMP@` →
   `ox_core::config::ox_home().join("tmp")`, substituted at test time. These
   two values are machine-dependent; every other byte is pinned.
   Pre-existing upstream divergence NOT pinned here: upstream renders
   `Platform: darwin` (Node `process.platform`), ox renders `macos` (Rust
   `std::env::consts::OS`); upstream's date value is `Date.toDateString()`,
   ox's is the ISO local calendar date (batch A, R4 — documented).

## Checksums (verified at test time; keep in sync when a fixture changes)

```
a0c7cb429411dc3455dfe85a23eb3b6ec06fab4f07de8f3e09350d82bface59d  date.txt
b10045312aaa6e0ccc3c3431682183e1316dcba033a6e7f589240b3dcd23a39d  env.txt
5936bfb214227b38f8d77b604b177f18a3892b2adf69df728400f441f91cf7e5  instruction-files.txt
8a325168f0851d79390c29593a5373b650522e668f3e98536dadc78fa5221d1c  skills.txt
ab9749036fe5a6b96c74c302900f67a9b249589b4fc4b8a8d18f3c6879dc506d  system.txt
```
