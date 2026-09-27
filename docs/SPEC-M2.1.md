# kioku — SPEC-M2.1: Antigravity CLI

Status: implemented 2026-09-27. Amends SPEC-M2; where this file and M2 disagree,
this file wins for the points it covers.

## 1. Why

Google retired Gemini CLI for personal accounts on 2026-06-18. The successor is
**Antigravity CLI** (`agy`), a closed-source Go binary (sources A1–A3). It does
**not** read `~/.gemini/settings.json` hooks (A5, A6), so kioku's Gemini CLI
integration (M2 §6) does nothing for those users. Gemini CLI still serves Code
Assist Standard/Enterprise and paid-API-key users, so M2 §6 stays as is.

M2.1 adds a fifth agent, `antigravity`, and fixes agent detection so a machine
with only Antigravity is no longer reported as having Gemini CLI (both keep
state under `~/.gemini`).

## 2. Sources (retrieved 2026-09-27)

| id | what | where |
|----|------|-------|
| A1 | Retirement notice | https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/ |
| A2 | Hooks doc | https://antigravity.google/docs/hooks/ |
| A3 | MCP doc (`serverUrl`) | https://antigravity.google/docs/mcp |
| A4 | Rules doc | https://antigravity.google/docs/rules/ |
| A5 | Live capture of agy 1.2.7 (macOS arm64, 2026-09-20): COMPATIBILITY.md + fixtures | https://github.com/automatis-tools/agents-can-communicate PR #181 (`packages/adapter-antigravity/`), issue #177 |
| A6 | claude-mem migration (settings.json hooks ignored; transcript format) | https://github.com/thedotmack/claude-mem/issues/4057, /issues/4196 |

A2 and A5 disagree; **A5 (a real capture) wins**. Everything marked
UNVERIFIED must be checked with `sh scripts/probe-agents.sh antigravity`.

## 3. Event model

### 3.1 Agent

`Agent::Antigravity`, `--agent antigravity`, session label `antigravity`.

### 3.2 Events

agy 1.2.7 loads exactly SessionStart, PreInvocation, PostInvocation and Stop
(A5); SessionEnd, PreToolUse, PostToolUse and Gemini names are silently
dropped. A2 documents Pre/PostToolUse, so kioku registers PostToolUse too
(dropped today, picked up if a later build fires it).

| neutral | Antigravity | notes |
|---|---|---|
| `session-start` | SessionStart | fires once per conversation (A5) |
| `user-prompt-submit` | PreInvocation | fires **once per model call** (a turn with two tool rounds fires three times, `invocationNum` 0,1,2); there is no prompt field — see §3.5 |
| `post-tool-use` | PostToolUse (`matcher: "*"`) | documented (A2), not fired by 1.2.7 |
| `stop` | Stop | end of every execution (turn) |
| `pre-compact` | — | none |
| `session-end` | — | none; Stop finalizes per turn (M2 behaviour) |

### 3.3 Payloads

camelCase, **no event-name field** (the event comes from argv) and **no cwd or
prompt field** (A5):

```json
// common
{"conversationId":"<uuid>","workspacePaths":["/Users/me/src/app"],
 "transcriptPath":"/Users/me/.gemini/antigravity-cli/brain/<uuid>/.system_generated/logs/transcript.jsonl",
 "artifactDirectoryPath":"…","modelName":"gemini-3.8-flash-high"}
// PreInvocation / PostInvocation: + "invocationNum":0, "initialNumSteps":0
// Stop: + "executionNum":0, "terminationReason":"NO_TOOL_CALL", "error":"", "fullyIdle":true
// PostToolUse (A2 only, UNVERIFIED): + "toolCall":{"name":…,"args":{…}}, "stepIdx":3, "error":""
```

- Session id: `conversationId`, then env `ANTIGRAVITY_CONVERSATION_ID` (the
  only variable agy passes, A5).
- cwd: `workspacePaths[0]`. Hooks run with the process cwd set to the
  directory of their `hooks.json` (A5): for the user file that is
  `~/.gemini/config`, which must never become a project. The process cwd is
  used only when it is **not** under `~/.gemini` (a workspace
  `.agents/hooks.json` runs in `<root>/.agents`, which identifies the right
  repository). Otherwise the event is dropped and logged, as for Cursor.
  `workspacePaths` is `[]` for `agy -p` without `--add-dir` (A5): those runs
  are not recorded — documented limitation.
- Tool events: `toolCall.name` is kept as the tool name, `toolCall.args` as the
  input, a non-empty `error` → `{is_error: true, error}`. Native tool names are
  UNVERIFIED, so there is no normalization table yet.

### 3.4 Output (A2, A5)

stdout is always exactly one JSON object and the exit code is always 0.

| result | SessionStart | PreInvocation | Stop |
|---|---|---|---|
| `Context(t)` | `{}` (context from SessionStart is undocumented; §3.6 delivers it) | `{"injectSteps":[{"ephemeralMessage":t}]}` (A5: the model sees it) | `{}` |
| `Nudge(m)` | — | — | `{"decision":"continue","reason":m}` (A5: reaches the model as `Stop hook blocked termination: …`) |
| `Silent` | `{}` | `{}` | `{}` |

### 3.5 Prompts from the transcript

PreInvocation has no prompt, so kioku reads it from `transcriptPath` (A6):
JSONL, one step per line; a user prompt is a line with `"type":"USER_INPUT"`
and the text in `content` (a string; `content.text` accepted too). On every
PreInvocation kioku reads at most the last 256 KiB, takes the **last**
USER_INPUT line, and records it as a prompt observation only if its end offset
is past the offset stored in `<kioku dir>/state/antigravity/<id>.prompt` (then
stores the new offset). A missing or unreadable transcript means no prompt;
it never fails the hook. The line format is UNVERIFIED beyond A6.

A 404 on that observation is an implicit start (M2 §3.9); its `<kioku>` block
is returned right away.

### 3.6 Handoff delivery (late context)

As for Cursor (M2 §5.6): the first PreInvocation of a conversation returns the
`<kioku>` block from `GET /sessions/{id}/context`, exactly once, guarded by an
O_EXCL marker `<kioku dir>/state/antigravity/<id>.ctx`; on error the marker is
removed so the next model call retries. SessionStart only starts the session
(and cleans markers older than 7 days).

### 3.7 Tool rounds and the Stop guard

The Stop nudge (M1 §7.1) needs ≥ 3 tool uses since the last handoff, and agy
1.2.7 fires no tool events. A PreInvocation with `invocationNum > 0` means the
previous model call ran tools, so kioku records it as one tool use named
`tool_round` (`native_tool: "PreInvocation"`, input `{invocationNum}`). It
counts toward the threshold and the session's tool total; the digest lists no
files or commands for it. If a later agy fires PostToolUse, both are counted —
acceptable, revisit then.


`executionNum` is 1 after a continue, but whether it counts per turn or per
conversation is UNVERIFIED, and agy caps consecutive continues itself (A5).
kioku keeps its own guard: a nudge creates `<kioku dir>/state/antigravity/<id>.nudge`;
a Stop that finds the file deletes it and counts as `stop_hook_active`
(finalize, no second nudge). Nudge text: `stop_nudge_generic`.

### 3.8 Deadlines

Registered timeouts are **seconds** (A2, default 30): SessionStart 10, Stop 10,
PreInvocation and PostToolUse 5. PreInvocation runs before every model call,
so it must stay fast: one bounded file read and an HTTP call only when there is
something to record or deliver (a new prompt, a tool round, the first call).

## 4. Installer

### 4.1 Hooks

User `~/.gemini/config/hooks.json`, project `<root>/.agents/hooks.json` (the
latter loads only when that directory is an open workspace, A5). The file's
top-level keys are named integration groups; kioku owns the group `kioku` and
replaces it wholesale. One unrecognised top-level key makes agy drop the
whole file, other tools' groups included (A5), so kioku never writes anything
else at the top level. Lifecycle events take a flat handler list, tool events
a `{matcher, hooks}` group:

```json
{
  "orca-status": { "…": "foreign group, kept as is" },
  "kioku": {
    "SessionStart":  [{ "type": "command", "command": "<bin> hook session-start --agent antigravity", "timeout": 10 }],
    "PreInvocation": [{ "type": "command", "command": "<bin> hook user-prompt-submit --agent antigravity", "timeout": 5 }],
    "PostToolUse":   [{ "matcher": "*", "hooks": [{ "type": "command", "command": "<bin> hook post-tool-use --agent antigravity", "timeout": 5 }] }],
    "Stop":          [{ "type": "command", "command": "<bin> hook stop --agent antigravity", "timeout": 10 }]
  }
}
```

Uninstall removes the `kioku` group (and the file if kioku created it and it
is then empty). `agy -p "/hooks" --output-format json` shows what loaded.

### 4.2 MCP

`~/.gemini/config/mcp_config.json` (user level only — the token never goes
into a repository; `~/.gemini/antigravity/mcp_config.json` is the legacy
desktop-app file and is not touched):

```json
{"mcpServers": {"kioku": {"serverUrl": "http://127.0.0.1:7391/mcp", "headers": {"Authorization": "Bearer <token>"}}}}
```

`url` / `httpUrl` are not supported by agy (A3).

### 4.3 Instructions

Written by default (hooks record nothing when `workspacePaths` is empty). User
`~/.gemini/GEMINI.md` — also Gemini CLI's file, so the existing shared-file
rule applies (the block stays while the other agent is installed); project
`<root>/AGENTS.md` (shared with Codex the same way).

### 4.4 Detection (fixes M2 §8.3 for `~/.gemini`)

| agent | detected when any of these exists |
|-------|-----------------------------------|
| gemini-cli | `~/.gemini/tmp` (Gemini CLI's per-project chat store) — **not** `~/.gemini`, which the Antigravity app and CLI create too |
| antigravity | `~/.gemini/antigravity-cli` (CLI state), `~/.local/bin/agy` (installer target) |

`~/.gemini/antigravity` belongs to the desktop app and does not count.

## 5. doctor

`agent.antigravity.hooks` and `agent.antigravity.mcp` work as for the other
agents (MCP URL key `serverUrl`); `agent.antigravity.instructions` as usual.
No enablement or trust check (none exists in agy).

## 6. Tests

- Parser: fixtures `tests/fixtures/antigravity/*.docs.json` built from A5's
  fixtures (anonymised) and A2: session id / cwd resolution incl. the
  `~/.gemini` process-cwd rule and the env fallback.
- Render goldens for §3.4; JSON-only stdout invariant extended.
- Installer: merge keeps a foreign group byte-for-byte, reinstall is
  idempotent, uninstall removes only `kioku`; MCP entry shape; detection
  table §4.4 (Gemini CLI not detected from `~/.gemini/antigravity`).
- e2e: session-start → PreInvocation (block injected once, prompt read from a
  transcript fixture and recorded once across three invocations) → Stop nudge
  → Stop finalizes; implicit start via PreInvocation.

## 7. UNVERIFIED (resolve with a real capture)

1. Transcript line format for USER_INPUT (§3.5), and whether the prompt is in
   the transcript before the first PreInvocation.
2. `executionNum` scope (§3.7).
3. Whether SessionStart output can inject context.
4. PostToolUse payload and native tool names (§3.3).
5. Whether empty stdout / non-zero exit codes are tolerated (kioku avoids both).
