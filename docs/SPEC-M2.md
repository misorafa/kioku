# kioku M2 — specification

Status: authoritative for milestone M2. M1 (`docs/SPEC-M1.md`) stays in force;
this document only adds to it or changes it where a section says so explicitly.
Later milestones (web UI, embeddings, LLM consolidation, ingest adapters, eval
harness) are out of scope and must not leak into M2 code paths.

Research date: 2026-09-26. Every agent-facing fact below cites its source
(§2). Anything the sources do not settle is marked **UNVERIFIED — check with a
captured payload** and collected again in §18; the implementation must not
guess past such a marker, it must use the `KIOKU_HOOK_DUMP` capture (§3.8) and
fix this spec in the same change.

## 1. Goal

M2 delivers exactly:

1. **Same memory from Codex CLI, Cursor (desktop + CLI) and Gemini CLI** when
   they open the same repository: per-agent stdin parsers and output renderers
   behind `kioku hook <event> --agent <name>`, automatic capture, SessionStart
   context injection, Stop-time finalize, a Stop nudge wherever the agent can
   feed a message back, MCP registration, and a small instruction snippet.
2. **One-command machine setup**: `kioku setup` (idempotent: init → user-level
   background service → hooks + MCP for every detected agent → summary),
   `kioku service …`, `kioku doctor`, `kioku setup --client-only <url> <token>`,
   a POSIX `install.sh` (`curl -fsSL … | sh`) that never needs sudo, and
   (optional) `kioku update`.
3. Small server/core additions needed by 1–2 (§9).

Not in M2: Windows support for service/installer (hooks/installers must not
break on Windows paths, but nothing is tested there), web UI, `.pkg`/`.deb`
packaging, Homebrew tap, writing Codex hook trust on the user's behalf (§4.7).

## 2. Sources (retrieved 2026-09-26)

| id | agent | what | URL |
|----|-------|------|-----|
| C1 | Codex | Hooks guide (events, config shape, stdin/stdout, timeouts, trust, feature flag) | https://developers.openai.com/codex/hooks |
| C2 | Codex | MCP guide (`[mcp_servers.<id>]`, `url`, `http_headers`, project scope) | https://developers.openai.com/codex/mcp |
| C3 | Codex | Config reference (`features.hooks`, `hooks.*`, `mcp_servers.<id>.http_headers`, `projects.<path>.trust_level`) | https://developers.openai.com/codex/config-reference |
| C4 | Codex | AGENTS.md discovery | https://developers.openai.com/codex/guides/agents-md |
| C5 | Codex | Generated hook wire schemas (`session-start.command.input.schema.json`, `stop.command.input…`, `post-tool-use…`, `user-prompt-submit…`, `session-end…`, `*.command.output…`) | https://github.com/openai/codex/tree/main/codex-rs/hooks/schema/generated |
| C6 | Codex | Feature registry: `FeatureSpec { key: "hooks", stage: Stable, default_enabled: true }` | https://github.com/openai/codex/blob/main/codex-rs/features/src/lib.rs |
| C7 | Codex | Hook trust state `[hooks.state.<key>] trusted_hash` | https://github.com/openai/codex/blob/main/codex-rs/hooks/src/config_rules.rs |
| C8 | Codex | Changelog: "Hooks general availability" (2026-05-14) | https://developers.openai.com/codex/changelog |
| U1 | Cursor | Hooks reference (events, `hooks.json`, common schema, per-event I/O, env vars, exit codes) | https://cursor.com/docs/hooks |
| U2 | Cursor | Third-party hooks (Cursor runs Claude Code hooks from `~/.claude/settings.json` etc.) | https://cursor.com/docs/reference/third-party-hooks |
| U3 | Cursor | MCP (`mcp.json`, `url`, `headers`, `${env:…}` interpolation, user/project paths) | https://cursor.com/docs/mcp |
| U4 | Cursor | CLI MCP (`agent mcp list`, same config as the editor) | https://cursor.com/docs/cli/mcp |
| U5 | Cursor | CLI usage (reads `.cursor/rules`, `AGENTS.md`, `CLAUDE.md`, `mcp.json`) | https://cursor.com/docs/cli/using |
| U6 | Cursor | CLI changelog (hooks incl. session start/end/stop in the CLI; Claude Code hooks merged; payloads over stdin) | https://cursor.com/docs/cli/changelog |
| U7 | Cursor | Rules (`.cursor/rules/*.mdc`, `alwaysApply`, User Rules are UI-only) | https://cursor.com/docs/rules |
| U8 | Cursor | CLI config / install (`agent` binary in `~/.local/bin`, `~/.cursor/cli-config.json`) | https://cursor.com/docs/cli/reference/configuration , https://cursor.com/docs/cli/installation |
| U9 | Cursor | Forum bug reports: sessionStart `additional_context` not reaching the model (topics 168441, 167274, 170135, 163990); empty `conversation_id` on tool events (167095); `additional_context` from `postToolUse` works | https://forum.cursor.com/t/168441 , https://forum.cursor.com/t/167274 , https://forum.cursor.com/t/167095 |
| G1 | Gemini | Hooks overview (events, exit codes, "stdout must be JSON", config layers, env vars, project hook fingerprinting) | https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/index.md |
| G2 | Gemini | Hooks reference (per-event input/output fields) | https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/reference.md |
| G3 | Gemini | Writing hooks / best practices (print `{}`; trust model) | https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/writing-hooks.md , …/best-practices.md |
| G4 | Gemini | Hook TypeScript types (`HookInput`, `SessionStartSource`, `SessionEndReason`, `AfterAgentInput`) | https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/hooks/types.ts |
| G5 | Gemini | Configuration reference (`hooksConfig.enabled` default true, `hooksConfig.disabled`, `mcpServers.<n>.httpUrl/headers`, `$VAR` expansion in settings, `context.fileName`) | https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md |
| G6 | Gemini | MCP servers (`httpUrl` → StreamableHTTP, `headers`, `gemini mcp add -t http -H …`) | https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/mcp-server.md |
| G7 | Gemini | GEMINI.md hierarchy (`~/.gemini/GEMINI.md`, workspace + ancestors) | https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/gemini-md.md |
| G8 | Gemini | Tool argument names (`read_file`/`write_file`/`replace` → `file_path`; `run_shell_command` → `command`) | https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/file-system.md , …/shell.md |

## 3. Neutral event model (changes to M1 §8.1)

### 3.1 Agents

`kioku_cli::event::Agent` gains three variants. The string is the `--agent`
value, the `sessions.agent` column and the session page `agent:` label.

| variant | `--agent` / label | parser |
|---------|-------------------|--------|
| `ClaudeCode` | `claude-code` | M1 (+ Cursor sniff, §3.7) |
| `Codex` | `codex` | §4 |
| `Cursor` | `cursor` (desktop and `agent` CLI alike) | §5 |
| `GeminiCli` | `gemini-cli` | §6 |

### 3.2 Event mapping

The CLI event stays the neutral kind (`kioku hook <neutral> --agent <a>`); the
parser may additionally branch on the payload's `hook_event_name` where one
neutral kind is registered for several native events (Cursor only).

| neutral (`kioku hook …`) | Claude Code | Codex | Cursor | Gemini CLI |
|---|---|---|---|---|
| `session-start` | SessionStart | SessionStart | sessionStart | SessionStart |
| `user-prompt-submit` | UserPromptSubmit | UserPromptSubmit | beforeSubmitPrompt | BeforeAgent |
| `post-tool-use` | PostToolUse | PostToolUse | postToolUse, postToolUseFailure, afterFileEdit | AfterTool |
| `stop` | Stop | Stop | stop | AfterAgent |
| `pre-compact` | PreCompact | PreCompact | preCompact | PreCompress |
| `session-end` | SessionEnd | SessionEnd | sessionEnd | SessionEnd |

Native events kioku does not register: Codex PreToolUse, PermissionRequest,
PostCompact, SubagentStart/Stop, Interrupt; Cursor preToolUse,
before/afterShellExecution, before/afterMCPExecution, beforeReadFile,
subagent*, afterAgentResponse/Thought, Tab hooks, workspaceOpen; Gemini
BeforeTool, BeforeModel, AfterModel, BeforeToolSelection, Notification.

### 3.3 `HookEvent` additions

```rust
pub struct HookEvent {
    // M1 fields unchanged: agent, event, session_id, cwd, source, prompt, tool_name,
    // tool_input, tool_response, tool_use_id, stop_hook_active, trigger, reason, raw
    /// Native event name as sent (`hook_event_name`), empty if absent.
    pub native_event: String,
    /// Workspace roots (Cursor `workspace_roots`); empty for other agents.
    pub workspace_roots: Vec<String>,
    /// Native tool name before normalization (§3.5), e.g. `apply_patch`, `Shell`, `run_shell_command`.
    pub native_tool: Option<String>,
    /// Turn id (Codex `turn_id`), informational.
    pub turn_id: Option<String>,
    /// Cursor stop `loop_count` (0 on the first stop of a follow-up chain).
    pub loop_count: Option<u32>,
    /// Cursor stop `status` (`completed` | `aborted` | `error`).
    pub stop_status: Option<String>,
}
```

`stop_hook_active` is the neutral "already continued by a Stop hook" flag:
Claude/Codex/Gemini `stop_hook_active`; Cursor `loop_count > 0`.

### 3.4 Session id and cwd resolution

Session id (first non-empty wins; if none → parse error, logged, exit 0):

| agent | order |
|-------|-------|
| claude-code, codex | `session_id` |
| gemini-cli | `session_id`, then env `GEMINI_SESSION_ID` (G1) |
| cursor | `conversation_id`, then `session_id` (U1: sessionStart/End carry `session_id` "same as conversation_id"; U9: `conversation_id` is intermittently empty on tool events) |

cwd (first non-empty, absolute path wins):

1. payload `cwd` (Claude, Codex, Gemini always; Cursor only on some events);
2. `workspace_roots[0]` (Cursor; a multi-root workspace uses the first root —
   documented limitation). **For Cursor steps 1 and 2 are swapped**: the
   workspace root wins over a payload `cwd`, which can be a subdirectory (or
   the hook's own `~/.cursor`) and would resolve to a different project;
3. environment: `CURSOR_PROJECT_DIR`, `GEMINI_PROJECT_DIR`, `GEMINI_CWD`,
   `CLAUDE_PROJECT_DIR` (the one(s) belonging to the agent, then the Claude alias;
   Codex documents none, so it goes straight to step 4); `C:\…` counts as absolute;
4. process cwd — **except for Cursor**: user-level Cursor hooks run with cwd
   `~/.cursor/` (U1), so a Cursor event with no root is dropped (logged) rather
   than filed under a bogus project.

### 3.5 Tool normalization (client side, before sanitization)

The M1 digest understands Claude tool names. Parsers map native tools to those
names so the core digest keeps one code path; the native name is kept in the
observation payload as `native_tool`.

| agent | native tool (payload) | → `tool_name` | → `tool_input` |
|-------|----------------------|---------------|----------------|
| codex | `Bash` (`tool_input.command`: string; accept an array and join with spaces) | `Bash` | `{command}` |
| codex | `apply_patch` (`tool_input.command` = patch text; also accepted: `tool_input` itself a string, `patch`/`input` keys, or an argv array whose element holds the patch) | `Edit` | `{file_paths:[…], patch:<first 4000 chars>}` — paths from lines `^\*\*\* (Add|Update|Delete) File: (.+)$` and `^\*\*\* Move to: (.+)$`, extracted **before** truncation/sanitization; relative paths are joined onto the resolved cwd so the digest can relativize them to the project root |
| cursor | `Shell` (`tool_input.command`, `working_directory`) | `Bash` | `{command}` |
| cursor | `Read` | `Read` | `{file_path}` from `tool_input.file_path` / `path` / `target_file` / `filePath` (key **UNVERIFIED — check with a captured payload**) |
| cursor | afterFileEdit (`file_path`, `edits[]`) | `Edit` | `{file_path}`; `tool_response` = `{edits: <count>}` |
| cursor | postToolUseFailure (`error_message`, `failure_type`) | as above per tool | `tool_response` = `{is_error:true, error:<error_message>, failure_type}` |
| gemini-cli | `run_shell_command` | `Bash` | `{command}` |
| gemini-cli | `write_file` | `Write` | `{file_path}` |
| gemini-cli | `replace` | `Edit` | `{file_path}` |
| gemini-cli | `read_file` | `Read` | `{file_path}` |

Gemini file tools take the path from `file_path`, else `absolute_path` / `path`
(tolerance for older builds). Any other native tool keeps its name and input.

`tool_response` normalization: Cursor `tool_output` is a JSON **string** (U1)
→ parse it, fall back to the raw string. Gemini `tool_response.error` present
(G2) → add `is_error: true`. Codex `tool_response` is kept as is.

Core digest change (§9.3): Edit/Write/MultiEdit entries may carry
`tool_input.file_paths: [String]` instead of `file_path`; each path counts one
edit.

### 3.6 Handler result and per-agent rendering

`run_hook` keeps its signature; internally handlers return a neutral

```rust
pub enum HookResult { Silent, Context(String), Nudge(String) }
```

and `render(agent, event, result) -> HookOutcome {stdout, stderr, exit_code}`:

| agent | SessionStart `Context(t)` | UserPromptSubmit `Context(t)` (implicit start, §3.9) | PostToolUse `Context(t)` (Cursor late context, §5.6) | Stop `Nudge(m)` | `Silent` |
|-------|---------------------------|------------------|----------------|-----------------|----------|
| claude-code | stdout `t`, exit 0 | stdout `t` | — | stderr `m`, exit 2 (M1) | nothing, exit 0 |
| codex | stdout `t` (plain text = developer context, C1) | stdout `t` (C1: plain text added as developer context) | — | stderr `m`, exit 2 (C1: continuation prompt using the reason) | nothing, exit 0 (C1: "Exit 0 with no output is treated as success"; Stop must not print plain text) |
| cursor | stdout `{"additional_context": t}` | — (beforeSubmitPrompt cannot add context, U1) → deliver on next postToolUse | stdout `{"additional_context": t}` | stdout `{"followup_message": m}`, exit 0 (U1) | beforeSubmitPrompt: `{"continue": true}`; all others `{}`; exit 0 |
| gemini-cli | stdout `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":t}}` | stdout `{"hookSpecificOutput":{"hookEventName":"BeforeAgent","additionalContext":t}}` | — | stdout `{"decision":"deny","reason":m}`, exit 0 (G2: AfterAgent deny → reason sent as a new prompt; exit 0 + JSON is the "preferred" path, G1) | stdout `{}`, exit 0 (G1: stdout must be JSON only) |

Errors stay fail-open (M1 §8.1): render `Silent` for the agent (Gemini/Cursor
still print `{}`) and exit 0. The process environment is read with
`std::env::vars_os()` (a non-UTF-8 name is skipped, a non-UTF-8 value converted
lossily — `kioku_core::util::env_vars`), so an odd environment can never panic
a hook (`std::env::vars()` would, exit 101 with a panic on stderr). Nudge only on Cursor `stop` with `status ==
"completed"` (aborted/error stops finalize without a nudge); a stop payload
without `status` (Claude-shaped, §3.7) counts as completed. JSON replies are
one object followed by `\n`.

Nudge text: new strings `stop_nudge_generic` (ja/en) in `strings.rs` for
non-Claude agents — same content as M1 §8.4 but the last sentence reads
「記録済みなら、そのまま終了してください。」 / "If you already did, just stop."
(no `stop_hook_active` reference). Claude keeps the M1 text.

### 3.7 Cursor sniff in the Claude Code parser

Cursor executes Claude Code hooks from `~/.claude/settings.json`,
`.claude/settings.json` and `.claude/settings.local.json` when "Include
Third-Party Plugins, Skills, and Other Configs" is on — **on by default** (U2).
A machine with kioku installed for Claude Code would therefore run
`kioku hook … --agent claude-code` inside Cursor too.

Rule: `--agent claude-code` with a payload that carries a Cursor field —
`cursor_version`, `conversation_id`, `workspace_roots`, or a camelCase
`hook_event_name` (Cursor's `postToolUse`; Claude's are PascalCase) — is a
Cursor invocation. The payload alone decides: `CURSOR_VERSION` in the
environment is **never** sufficient (Claude Code started from Cursor's
integrated terminal inherits it and must stay Claude Code):

- if a kioku entry exists in `~/.cursor/hooks.json` or in
  `<workspace_roots[0]>/.cursor/hooks.json` → `Silent` (the native Cursor hook
  handles it; avoids double capture);
- else → re-dispatch to the Cursor parser/renderer (Cursor still works through
  the imported Claude hooks).

Which payload Cursor sends to an imported Claude hook (native Cursor fields or
Claude-shaped) is **UNVERIFIED — check with a captured payload**; the Cursor
parser must accept both `session_id`/`cwd` and `conversation_id`/`workspace_roots`.
A purely Claude-shaped payload from Cursor is handled as Claude Code (it is
indistinguishable from one).

### 3.8 `KIOKU_HOOK_DUMP` (payload capture)

When `KIOKU_HOOK_DUMP=1` is in the hook's environment **or** `[client]
hook_dump = true` in config.toml (GUI apps such as Cursor do not inherit shell
env), every `kioku hook` invocation appends one JSON line to
`<log dir>/hook-dump.jsonl` (log dir as in M1 §8.1) **before parsing**:

```json
{"ts":"2026-09-26T03:00:00Z","agent":"cursor","event":"stop","argv":["hook","stop","--agent","cursor"],
 "cwd":"/Users/me/.cursor","env":{"CURSOR_PROJECT_DIR":"/Users/me/src/app","CURSOR_VERSION":"3.1.0"},
 "stdin":"<raw stdin, verbatim string>","outcome":{"exit_code":0,"stdout":"{}","stderr":""}}
```

(`outcome` is filled after handling; write the line once, at exit.)

- `env` contains only variables whose names start with `CURSOR_`, `GEMINI_`,
  `CODEX_`, `CLAUDE_`, plus `KIOKU_*` except `KIOKU_AUTH_TOKEN`. Values of names
  containing `TOKEN`, `SECRET`, `PASSW`, `API_KEY`/`APIKEY`, `CREDENTIAL` or
  `AUTH` are replaced by `[REDACTED]` (agents export e.g.
  `CLAUDE_CODE_OAUTH_TOKEN`; the names are what matters for capture).
- `stdin` is raw (unsanitized — the point is capturing exact shapes). The file
  is created 0600, rotated like `hook.log` but at 5 MiB. Dumping never changes
  the outcome and never fails the hook.
- `kioku doctor` warns while dumping is on (§12). `kioku hook-dump extract
  <agent> <event> [--out dir]` (small helper) writes the newest matching
  `stdin` as a pretty-printed fixture `<out>/<agent>/<event>.captured.json`
  (`--out` defaults to the cwd), so captured payloads replace the docs-derived
  fixtures (§16). `<event>` matches the neutral event or the native
  `hook_event_name` (`post-tool-use`, `afterFileEdit`, `BeforeAgent`, …) and is
  snake_cased for the file name (`after_file_edit`); `hook-dump.jsonl.1` is
  searched after the current file.

### 3.9 Implicit session start

When an observation POST or Stop GET returns 404 (unknown session — SessionStart
did not fire: Codex hooks not yet trusted, Cursor cloud agents, agent started
before `kioku install`, Cursor sessionStart race), the hook:

1. calls `POST /sessions/start` with `source = "implicit"` and the resolved cwd;
2. retries the original request once;
3. on `user-prompt-submit`, returns `Context(<§8.3 block>)` so agents that can
   inject at prompt time (Claude, Codex, Gemini) show the handoff; for Cursor
   the block is delivered by the late-context path (§5.6).

Stop on an unknown session after implicit start has nothing to finalize →
`Silent`. At most one implicit start per invocation. This applies to Claude Code
too and replaces M1 §9's "unknown session on `observations` → hook silently
drops" (the M1 e2e test was updated accordingly). SessionEnd on an unknown
session is `Silent` and **not** logged: there is nothing to finalize, and it is
the normal case for Codex, whose SessionStart only fires with the first turn —
opening Codex and quitting without a prompt sends SessionEnd alone (seen in real
captures 2026-09-27; it used to put a 404 in `hook.log` and a doctor WARN).
A finalize (Stop or SessionEnd) whose request was sent but got no answer before the
hook's deadline is `Silent` and **not** logged either: the server finishes it on its
own (digest, git commits) and the session ends up finalized. On Windows that work
regularly outlives the default 3 s `timeout_ms` (CI runners, 2026-09-28/29), which put
an error in `hook.log` — and a doctor WARN — for every such Stop. A connection that
fails, or an error response, is still logged.

### 3.10 Per-agent deadlines

M1's single deadline stays `timeout_ms`, capped per invocation at the
registered agent timeout minus 500 ms: Codex SessionEnd 2 500 ms (C1: SessionEnd
max 3 s); all other registrations use ≥ 5 s so `timeout_ms` (3 000) applies.
The registered timeouts live in `event::registered_timeout_ms` (Claude Code:
10 s SessionStart as installed, else its 60 s default) — Step 3's installers
must write the same values.

## 4. Codex CLI

### 4.1 Files and enablement

- Codex home: `$CODEX_HOME`, default `~/.codex` (C3, C4).
- Hooks: `~/.codex/hooks.json` (user), `<repo>/.codex/hooks.json` (project);
  inline `[hooks]` in `config.toml` is equivalent. All sources load; a layer
  with both forms merges and **warns at startup** (C1). kioku always writes
  `hooks.json`; if the same layer's `config.toml` already has inline `[hooks]`,
  install and doctor print an info line about Codex's warning (harmless).
- Project hooks load only when the project `.codex/` layer is trusted
  (`projects.<path>.trust_level = "trusted"`, C1/C3).
- **Feature flag (verified)**: hooks are **on by default**; `[features] hooks =
  false` turns them off; `codex_hooks` is a deprecated alias (C1, C3). Source:
  `FeatureSpec { key: "hooks", stage: Stable, default_enabled: true }` (C6);
  GA announced 2026-05-14 (C8). Older Codex builds gated hooks behind
  `[features] codex_hooks = true`; the first version with default-on is
  **UNVERIFIED** (record `codex --version` in doctor). kioku does not write
  `[features]` by default; `kioku install codex --enable-hooks-feature` appends
  `hooks = true` into `[features]` via the managed block (§4.6) for old builds.
  (Step 3: TOML forbids defining `[features]` twice, so when the file already has
  its own `[features]` table the block omits it and install prints "add
  `hooks = true` under it yourself". Once written, the feature line is kept on
  later installs without the flag — it lives inside our block.)
  If `features.hooks` or `features.codex_hooks` is `false`, install warns and
  doctor fails the check.
- **Trust review (verified)**: non-managed hooks must be reviewed and trusted
  before they run; trust is recorded against the hook definition's hash, so a
  new or changed hook is skipped until trusted via `/hooks` in the TUI (C1).
  State lives in `config.toml` as `[hooks.state."<key>"] trusted_hash =
  "sha256:…"` (C7); the key format and hash input are **UNVERIFIED**. kioku
  never writes trust. Consequences: (a) `setup`/`install` print "open Codex and
  run /hooks once to trust kioku's hooks"; (b) the hook command string must be
  stable across updates (absolute path in `~/.local/bin`, §13) so re-trust is
  only needed when the path changes; (c) until trusted, hooks do not run — the
  AGENTS.md snippet (§7) and implicit start (§3.9) cover that window.
  `--dangerously-bypass-hook-trust` exists for one-off automation only.

### 4.2 Hook config shape (verified, C1)

Same three-level structure as Claude Code: event → matcher groups → handlers.
`timeout` in **seconds** (default 600; SessionEnd/Interrupt default 1, max 3).
Handler fields used: `type` (`"command"`), `command`, `timeout`,
`statusMessage`, `additionalContextLimit` (tokens before Codex spills
additional context to a file; default ≈ 2 500; `0` = pass through). Commands run
with the session cwd.

What `kioku install codex` writes (merged into `hooks.json`; `<bin>` absolute):

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "<bin> hook session-start --agent codex",
                                   "timeout": 10, "statusMessage": "kioku: loading handoff",
                                   "additionalContextLimit": 0 }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "<bin> hook user-prompt-submit --agent codex", "timeout": 5 }] }],
    "PostToolUse": [{ "matcher": "^(Bash|shell|exec_command|apply_patch)$",
                      "hooks": [{ "type": "command", "command": "<bin> hook post-tool-use --agent codex", "timeout": 5 }] }],
    "PreCompact": [{ "hooks": [{ "type": "command", "command": "<bin> hook pre-compact --agent codex", "timeout": 5 }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "<bin> hook stop --agent codex", "timeout": 10 }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "<bin> hook session-end --agent codex", "timeout": 3 }] }]
  }
}
```

`additionalContextLimit: 0` is deliberate: kioku caps the SessionStart block at
6 000 chars itself (M1 §8.3), and a Japanese block of that size can exceed the
2 500-token default and would otherwise be replaced by a head/tail preview.
SessionStart has no matcher (all sources, including `fork`, C5). Matchers on
UserPromptSubmit/Stop are ignored by Codex anyway (C1).

### 4.3 Stdin payloads (verified from C1 + C5 schemas)

Common: `session_id` (subagent hooks use the parent's), `transcript_path`
(string|null), `cwd`, `hook_event_name`, `model`; `permission_mode` on
SessionStart/UserPromptSubmit/PostToolUse/Stop; `turn_id` on turn-scoped
events. Examples (assembled from the schemas; replace with captured ones):

```json
// SessionStart — source ∈ startup|resume|clear|compact|fork (C5 enum; the guide lists the first four)
{"session_id":"019a…","transcript_path":"/Users/me/.codex/sessions/2026/09/26/rollout-….jsonl","cwd":"/Users/me/src/kioku",
 "hook_event_name":"SessionStart","model":"gpt-5-codex","permission_mode":"default","source":"startup"}
// UserPromptSubmit
{"session_id":"019a…","transcript_path":null,"cwd":"/Users/me/src/kioku","hook_event_name":"UserPromptSubmit",
 "model":"gpt-5-codex","permission_mode":"default","turn_id":"turn_3","prompt":"引き継ぎの自動化を続けて"}
// PostToolUse (Bash)
{"session_id":"019a…","transcript_path":null,"cwd":"/Users/me/src/kioku","hook_event_name":"PostToolUse","model":"gpt-5-codex",
 "permission_mode":"default","turn_id":"turn_3","tool_name":"Bash","tool_use_id":"call_Ab12",
 "tool_input":{"command":"cargo test -p kioku-core"},"tool_response":"test result: ok. 126 passed"}
// PostToolUse (apply_patch) — tool_input.command carries the patch
{"…":"…","tool_name":"apply_patch","tool_use_id":"call_Cd34",
 "tool_input":{"command":"*** Begin Patch\n*** Update File: crates/kioku-cli/src/event.rs\n@@ …\n*** End Patch"},
 "tool_response":"Success. Updated the following files:\nM crates/kioku-cli/src/event.rs"}
// PreCompact — trigger ∈ manual|auto
{"session_id":"019a…","transcript_path":null,"cwd":"/Users/me/src/kioku","hook_event_name":"PreCompact","model":"gpt-5-codex","turn_id":"turn_9","trigger":"auto"}
// Stop
{"session_id":"019a…","transcript_path":null,"cwd":"/Users/me/src/kioku","hook_event_name":"Stop","model":"gpt-5-codex",
 "permission_mode":"default","turn_id":"turn_3","stop_hook_active":false,"last_assistant_message":"Done."}
// SessionEnd — reason is always "other" today (C1)
{"session_id":"thr_123","transcript_path":"/workspace/.codex/rollout.jsonl","cwd":"/workspace","hook_event_name":"SessionEnd","reason":"other"}
```

The shape of `tool_response` for Bash/apply_patch (string vs object) is
**UNVERIFIED — check with a captured payload**; the normalizer accepts any JSON.

### 4.4 Output semantics (verified, C1)

- SessionStart: plain-text stdout → developer context; or JSON
  `hookSpecificOutput.additionalContext`. kioku prints plain text.
- UserPromptSubmit: plain-text stdout is **also** added as developer context →
  kioku prints nothing except for an implicit start (§3.9).
- Stop: exit 0 must print JSON or nothing ("plain text output is invalid");
  exit 2 + stderr (or `{"decision":"block","reason":…}`) makes Codex continue
  with the reason as a new continuation prompt; `stop_hook_active` is true on
  that continued turn → M1 nudge rule applies unchanged.
- SessionEnd is advisory, synchronous, ≤ 3 s; fires on normal close,
  archive/delete of an open conversation, or 30 min idle — **not** for
  subagents. Stop fires per turn, so finalize already ran; SessionEnd is the
  safety net.

### 4.5 MCP (verified, C2/C3)

`~/.codex/config.toml` (`$CODEX_HOME/config.toml`):

```toml
# >>> kioku (managed by `kioku install codex`; edits inside this block are overwritten) >>>
[mcp_servers.kioku]
url = "http://127.0.0.1:7391/mcp"
http_headers = { Authorization = "Bearer <token>" }
# <<< kioku <<<
```

`url` = streamable HTTP; `http_headers` = static header map (C2).
Alternatives Codex offers and kioku does not use by default:
`bearer_token_env_var = "KIOKU_AUTH_TOKEN"` (would need the variable in every
Codex launch environment); `codex mcp add` (stdio-oriented; would put the token
on argv). Project scope `.codex/config.toml` exists (trusted projects only) but
**kioku never writes the token inside a repository** (§8.1).

### 4.6 TOML editing without a new dependency

The workspace has `toml` (parse/serialize) but not a format-preserving editor.
kioku edits `config.toml` only through the delimited managed block above:

1. Parse the file with `toml` (unparseable → leave untouched, print snippet).
2. If the managed block exists → replace the text between the markers.
3. Else if `mcp_servers.kioku` exists (foreign definition) → leave untouched,
   warn, print snippet.
4. Else append `\n` + block at EOF (a `[table]` header at EOF is valid TOML
   regardless of what precedes it).
5. Re-parse the result; require `mcp_servers.kioku.url` to equal the expected
   URL, else abort without writing. Write via temp file + rename; keep the mode
   of an existing file, 0600 for a new one; backup `config.toml.kioku-bak` once.

Codex itself edits `config.toml` with `toml_edit`, which appends new tables
(`[projects."<path>"] trust_level`, `[hooks.state.*] trusted_hash`) **before the
document's trailing comment** — our end marker — i.e. inside the block when the
block is last. So before every install / uninstall the block is normalized:
only `[mcp_servers.kioku]` (and sub-tables) and a `[features]` table holding
nothing but `hooks = true` are kioku's; every other table inside the markers is
moved verbatim to just after the end marker (after one blank line), and key
lines before the block's first header move to just before the begin marker.
Nothing another writer put there is ever deleted.

Uninstall removes the block (and the one blank line before it). Bytes outside
the block are never changed. (`toml_edit` would be the alternative; not added.)
A begin marker without an end marker → file untouched, snippet printed. A file
that did not end with `\n` gets one before the appended block, so uninstall
restores it with a trailing newline (the only byte that can differ).

### 4.7 Instructions (verified, C4)

Global: `$CODEX_HOME/AGENTS.override.md` if it exists and is non-empty
(Codex then ignores `AGENTS.md` at that level), else `$CODEX_HOME/AGENTS.md`.
Project (`--project`): `<git root>/AGENTS.md`. Size budget: Codex stops at
`project_doc_max_bytes` (32 KiB) combined — the snippet is < 1 KiB.

### 4.8 Project variant

`kioku install codex --project`: `<git root or cwd>/.codex/hooks.json` +
`<git root>/AGENTS.md` block. MCP stays user-level. Print that the project must
be trusted in Codex for project hooks to load.

## 5. Cursor (desktop and `agent` CLI)

### 5.1 Files (verified, U1/U5/U6/U8)

- Hooks: `~/.cursor/hooks.json` (user; commands run with cwd `~/.cursor/`),
  `<project>/.cursor/hooks.json` (project; cwd = project root; only in trusted
  workspaces). Enterprise: `/Library/Application Support/Cursor/hooks.json`,
  `/etc/cursor/hooks.json` (not touched). All sources run. Cursor watches the
  files and reloads on save.
- The CLI (binary `agent`, installed to `~/.local/bin`; older name
  `cursor-agent`) uses the same `hooks.json`, `mcp.json`, `.cursor/rules`, and
  also reads `AGENTS.md` and `CLAUDE.md` at the project root (U5, U6).
- Cloud agents: user-level hooks and sessionStart/sessionEnd do not run (U1).
  Not a kioku target; implicit start (§3.9) covers project hooks there if a
  user commits them (not recommended: absolute machine paths).
- No feature flag. Detection dir: `~/.cursor`.

### 5.2 Hook config shape (verified, U1)

Flat: `{"version": 1, "hooks": {"<event>": [ {handler}, … ]}}` — no matcher
group nesting. Handler fields: `command` (required), `type` (`"command"`
default), `timeout` (**seconds**, "platform default" — value **UNVERIFIED**),
`matcher` (regex), `loop_limit` (stop/subagentStop; default 5), `failClosed`
(default false). kioku writes (merging into existing `hooks`, setting
`"version": 1` if absent):

```json
{
  "version": 1,
  "hooks": {
    "sessionStart":       [{ "command": "<bin> hook session-start --agent cursor", "timeout": 10 }],
    "beforeSubmitPrompt": [{ "command": "<bin> hook user-prompt-submit --agent cursor", "timeout": 5 }],
    "postToolUse":        [{ "command": "<bin> hook post-tool-use --agent cursor", "matcher": "Shell|Read", "timeout": 5 }],
    "postToolUseFailure": [{ "command": "<bin> hook post-tool-use --agent cursor", "matcher": "Shell", "timeout": 5 }],
    "afterFileEdit":      [{ "command": "<bin> hook post-tool-use --agent cursor", "timeout": 5 }],
    "preCompact":         [{ "command": "<bin> hook pre-compact --agent cursor", "timeout": 5 }],
    "stop":               [{ "command": "<bin> hook stop --agent cursor", "timeout": 10 }],
    "sessionEnd":         [{ "command": "<bin> hook session-end --agent cursor", "timeout": 5 }]
  }
}
```

Edits are captured by `afterFileEdit` (documented `file_path`), not by
`postToolUse` Write, to avoid counting an edit twice and because the Write
tool's `tool_input` keys are not documented. `failClosed` stays false (kioku is
observational; fail-open).

Exit codes (U1): 0 = use JSON output; 2 = block (like `permission: "deny"`);
other = failure, action proceeds. kioku never exits 2 for Cursor.

### 5.3 Stdin payloads (verified field lists, U1)

Common to all agent hooks: `conversation_id`, `generation_id`, `model`,
`model_id`?, `model_params`?, `hook_event_name`, `cursor_version`,
`workspace_roots` (string[]), `user_email` (string|null), `transcript_path`
(string|null). Env: `CURSOR_PROJECT_DIR` (always), `CURSOR_VERSION`,
`CURSOR_TRANSCRIPT_PATH`, `CLAUDE_PROJECT_DIR` (alias). Examples (common fields
shown once, `…` elsewhere):

```json
// sessionStart — fire-and-forget; carries session_id (= conversation_id); no cwd
{"conversation_id":"c7e1…","generation_id":"g-01","model":"composer-2.5","hook_event_name":"sessionStart",
 "cursor_version":"3.1.0","workspace_roots":["/Users/me/src/kioku"],"user_email":null,"transcript_path":null,
 "session_id":"c7e1…","is_background_agent":false,"composer_mode":"agent"}
// beforeSubmitPrompt
{"…":"…","hook_event_name":"beforeSubmitPrompt","prompt":"引き継ぎを確認して","attachments":[]}
// postToolUse (Shell) — tool_output is a JSON *string*
{"…":"…","hook_event_name":"postToolUse","tool_name":"Shell","tool_input":{"command":"npm test","working_directory":"/Users/me/src/app"},
 "tool_output":"{\"exitCode\":0,\"stdout\":\"All tests passed\"}","tool_use_id":"abc123","cwd":"/Users/me/src/app","duration":5432}
// postToolUseFailure
{"…":"…","hook_event_name":"postToolUseFailure","tool_name":"Shell","tool_input":{"command":"npm test"},"tool_use_id":"abc123",
 "cwd":"/Users/me/src/app","error_message":"Command timed out after 30s","failure_type":"timeout","duration":5000,"is_interrupt":false}
// afterFileEdit
{"…":"…","hook_event_name":"afterFileEdit","file_path":"/Users/me/src/app/src/auth.ts","edits":[{"old_string":"a","new_string":"b"}]}
// preCompact — observational only
{"…":"…","hook_event_name":"preCompact","trigger":"auto","context_usage_percent":85,"context_tokens":120000,
 "context_window_size":128000,"message_count":45,"messages_to_compact":30,"is_first_compaction":true}
// stop
{"…":"…","hook_event_name":"stop","status":"completed","loop_count":0}
// sessionEnd — fire-and-forget; response ignored
{"…":"…","hook_event_name":"sessionEnd","session_id":"c7e1…","reason":"user_close","duration_ms":45000,
 "is_background_agent":false,"final_status":"completed"}
```

Tool names for matchers: `Shell`, `Read`, `Write`, `Grep`, `Delete`, `Task`,
`MCP:<tool>` (U1). Whether the common fields really appear on every event
(U1 says "all hooks") and whether `cwd` is present beyond pre/postToolUse is
**UNVERIFIED — check with a captured payload**.

### 5.4 Output semantics (verified, U1/U2)

- sessionStart: `{"env": {…}, "additional_context": "…"}` — documented as
  "additional context to add to the conversation's initial system context", but
  the hook is fire-and-forget and several reports (U9, Aug 2026) show the text
  not reaching the model. Treat as **best effort**; §5.6 is the reliable path.
- postToolUse: `{"additional_context": "…"}` — "extra context injected into the
  conversation after the tool result" (U1; reported working, U9).
- beforeSubmitPrompt: `{"continue": bool, "user_message"?}` — no context field.
- stop: `{"followup_message": "…"}` → Cursor auto-submits it as the next user
  message; `loop_count` = follow-ups already triggered, capped by `loop_limit`
  (default 5). Claude-style `{"decision":"block","reason":…}` is accepted as an
  equivalent (U2). kioku uses `followup_message`; `stop_hook_active :=
  loop_count > 0`. Whether `loop_count` resets per user turn or accumulates per
  conversation is **UNVERIFIED** (if it accumulates, the nudge fires at most
  once per conversation — acceptable).
- sessionEnd: response ignored.

### 5.5 MCP (verified, U3/U4)

`~/.cursor/mcp.json` (user; CLI auto-approves global servers, U6) — kioku sets:

```json
{"mcpServers": {"kioku": {"url": "http://127.0.0.1:7391/mcp", "headers": {"Authorization": "Bearer <token>"}}}}
```

No `type` field is needed for remote servers (U3 example). Project
`.cursor/mcp.json` exists and supports `${env:NAME}` interpolation in `url` and
`headers`; kioku does not write it (§8.1). Whether Cursor's third-party import
also loads `mcpServers.kioku` from `~/.claude.json` (duplicate server) is
**UNVERIFIED**; the CLI dedupes the same remote server across *Cursor* scopes
(U6).

### 5.6 Late context (Cursor only)

Because sessionStart context is unreliable, the first native `postToolUse` of
each Cursor session delivers the kioku block through `additional_context`
(`afterFileEdit` and `postToolUseFailure` also run `post-tool-use` but cannot
carry it: they are recorded, and the marker stays unconsumed for the next
`postToolUse`; a Claude-shaped PostToolUse from imported hooks qualifies unless
it is an edit or a failure):

- marker `<data_dir>/state/cursor-ctx/<session_id>` (data dir chosen like the
  log dir in M1 §8.1: `~/.kioku` on a client-only machine), created with `create_new` (O_EXCL); if it
  already exists → normal `Silent` handling;
- on creation: record the observation as usual, then `GET
  /api/v1/sessions/{id}/context` (§9.1), render the M1 §8.3 block, return
  `Context(block)` → `{"additional_context": block}`;
- markers older than 7 days are deleted by the Cursor sessionStart handler;
- `[client] cursor_late_context = true` (default) toggles it. The block may
  appear twice when sessionStart delivery works — accepted until Cursor fixes
  U9; revisit after capture.

### 5.7 Instructions (verified, U7)

User Rules are UI-only (no file). Project (`--project`):
`<root>/.cursor/rules/kioku.mdc`, owned entirely by kioku:

```markdown
---
description: kioku shared memory — read the handoff at start, write one before finishing
alwaysApply: true
---
<snippet §7, with the project id filled in>
```

Cursor also reads `AGENTS.md`; kioku does not write it for Cursor (avoid
duplicate text when Codex's `--project` snippet is there too).

## 6. Gemini CLI

> **2026-09-27:** Google retired Gemini CLI for personal accounts on
> 2026-06-18; its successor Antigravity CLI is specified in `SPEC-M2.1.md`,
> which also moves Gemini CLI detection from `~/.gemini` to `~/.gemini/tmp`
> (§8.3). This section still applies to Gemini CLI itself.

### 6.1 Files and enablement (verified, G1/G5)

- Settings layers: `.gemini/settings.json` (project), `~/.gemini/settings.json`
  (user), `/etc/gemini-cli/settings.json` (system), extensions. Hooks live in
  the `hooks` key; all layers merge.
- `hooksConfig.enabled` (default **true**, restart required) is the global
  switch; `hooksConfig.disabled` lists hook names to skip. No opt-in needed.
- Project hooks are fingerprinted by `name` + `command`; a new/changed one
  triggers a warning before running (G1, G3). User hooks: no trust step.
- String values in settings.json undergo `$VAR` / `${VAR}` expansion (G5) —
  hook commands and headers must not contain `$` (absolute binary paths with
  `$` are rejected by the installer).
- `/hooks panel`, `/hooks enable|disable <name>` manage hooks by `name`.

### 6.2 Hook config shape (verified, G1/G2)

Nested like Claude Code; handler `timeout` in **milliseconds** (default 60 000);
`name` recommended. Lifecycle matchers are exact strings, tool matchers regexes.

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "name": "kioku-session-start", "type": "command",
                                   "command": "<bin> hook session-start --agent gemini-cli", "timeout": 10000 }] }],
    "BeforeAgent":  [{ "hooks": [{ "name": "kioku-user-prompt", "type": "command",
                                   "command": "<bin> hook user-prompt-submit --agent gemini-cli", "timeout": 5000 }] }],
    "AfterTool":    [{ "matcher": "run_shell_command|write_file|replace|read_file",
                       "hooks": [{ "name": "kioku-post-tool", "type": "command",
                                   "command": "<bin> hook post-tool-use --agent gemini-cli", "timeout": 5000 }] }],
    "PreCompress":  [{ "hooks": [{ "name": "kioku-pre-compact", "type": "command",
                                   "command": "<bin> hook pre-compact --agent gemini-cli", "timeout": 5000 }] }],
    "AfterAgent":   [{ "hooks": [{ "name": "kioku-stop", "type": "command",
                                   "command": "<bin> hook stop --agent gemini-cli", "timeout": 10000 }] }],
    "SessionEnd":   [{ "hooks": [{ "name": "kioku-session-end", "type": "command",
                                   "command": "<bin> hook session-end --agent gemini-cli", "timeout": 5000 }] }]
  }
}
```

### 6.3 Stdin payloads (verified, G2/G4)

Common (`HookInput`, G4): `session_id`, `transcript_path`, `cwd`,
`hook_event_name`, `timestamp`. Env: `GEMINI_PROJECT_DIR`, `GEMINI_SESSION_ID`,
`GEMINI_CWD`, `GEMINI_PLANS_DIR`, `CLAUDE_PROJECT_DIR`.

```json
// SessionStart — source ∈ startup|resume|clear
{"session_id":"5f0c…","transcript_path":"/Users/me/.gemini/tmp/…/chats/session-….json","cwd":"/Users/me/src/kioku",
 "hook_event_name":"SessionStart","timestamp":"2026-09-26T03:00:00.000Z","source":"startup"}
// BeforeAgent
{"…":"…","hook_event_name":"BeforeAgent","prompt":"STATE.md を読んで続きをやって"}
// AfterTool — tool_response {llmContent, returnDisplay, error?}
{"…":"…","hook_event_name":"AfterTool","tool_name":"replace",
 "tool_input":{"file_path":"/Users/me/src/kioku/src/lib.rs","old_string":"a","new_string":"b"},
 "tool_response":{"llmContent":"Successfully modified file","returnDisplay":"…"}}
// AfterAgent
{"…":"…","hook_event_name":"AfterAgent","prompt":"…","prompt_response":"完了しました。","stop_hook_active":false}
// PreCompress — trigger ∈ manual|auto; fired asynchronously, advisory
{"…":"…","hook_event_name":"PreCompress","trigger":"auto"}
// SessionEnd — reason ∈ exit|clear|logout|prompt_input_exit|other; CLI does not wait
{"…":"…","hook_event_name":"SessionEnd","reason":"exit"}
```

`tool_response` for `run_shell_command` (exact `llmContent` text) is
**UNVERIFIED — check with a captured payload**; the digest only needs the
command and `error`.

### 6.4 Output semantics (verified, G1/G2)

- stdout must contain **only** the final JSON object; non-JSON stdout breaks
  parsing (defaults to allow and shows the text as `systemMessage`). kioku
  prints `{}` for silent success (G3 examples do the same).
- SessionStart: `hookSpecificOutput.additionalContext` — interactive: injected
  as the first turn in history; non-interactive: prepended to the prompt.
  Advisory; never blocks startup.
- BeforeAgent: `hookSpecificOutput.additionalContext` appended to the prompt
  for this turn (used by implicit start only).
- AfterAgent: `{"decision":"deny","reason":…}` rejects the response and sends
  `reason` to the agent as a new prompt; `stop_hook_active` is true on the
  retry. Exit 2 + stderr does the same (G2).
- SessionEnd: best effort, the CLI does not wait → finalize may be cut off;
  AfterAgent already finalized the turn.

### 6.5 MCP (verified, G5/G6)

`~/.gemini/settings.json`:

```json
{"mcpServers": {"kioku": {"httpUrl": "http://127.0.0.1:7391/mcp", "headers": {"Authorization": "Bearer <token>"}, "timeout": 10000}}}
```

`httpUrl` selects streamable HTTP (`url` would mean SSE; precedence `httpUrl` >
`url` > `command`). The alias `kioku` has no underscore (G5 warns underscores
break FQN parsing; tools appear as `mcp_kioku_kioku_query` etc.). `trust` is
not set (tool calls keep Gemini's confirmation); `--trust-mcp` sets `"trust":
true`. `gemini mcp add -t http -H …` exists but would put the token on argv.

### 6.6 Instructions (verified, G5/G7)

Global `~/.gemini/GEMINI.md`; project `<root>/GEMINI.md`. If
`context.fileName` is set (string or array) and does not include `GEMINI.md`,
use its first entry instead.

### 6.7 Project variant

`kioku install gemini-cli --project`: hooks into `<root>/.gemini/settings.json`
(+ `GEMINI.md` block). Print that Gemini will show a one-time untrusted-hook
warning for project hooks.

## 7. Instruction snippet

A tiny, language-aware (`[client] lang`) block telling the agent to use kioku
even when hooks are not (yet) active. Written by default for Codex (global
AGENTS.md — covers the trust window), Gemini (global GEMINI.md) and Cursor
(project rule, `--project` only); not for Claude Code (M1 unchanged).
`--no-instructions` skips it; `--instructions` forces it for Claude
(`~/.claude/CLAUDE.md`).

Markdown files (AGENTS.md, GEMINI.md, CLAUDE.md) get a delimited block, replaced
in place on reinstall and removed on uninstall (file deleted only if kioku
created it and it is empty afterwards):

```markdown
<!-- kioku:begin v1 (managed by `kioku install`; edits inside are overwritten) -->
## kioku（共有メモリ）
- kioku の MCP ツール（kioku_*）はこのマシンと他のエージェントで共有される記憶。
- セッション開始時に `<kioku>` ブロックがあれば、その project / session を使う。無ければ作業前に
  `kioku_read` で `<project>/STATE.md` を読む（project id: {project_line}）。
- 調べる前に `kioku_query` で過去の記録を検索する。
- タスクを終える前に必ず `kioku_handoff_write`（project, session, summary, next_steps,
  open_questions, decisions）で引き継ぎを書く。session が分からなければ省略してよい。
<!-- kioku:end -->
```

`{project_line}`: project-level snippets embed the id from `identify(root)`
(e.g. `chord-life-ace9dc4a`); global snippets say「`kioku project id` の出力」/
"the output of `kioku project id`". English variant has the same four bullets.
Size < 1 KiB.

## 8. Installers

### 8.1 Common rules (extend M1 §8.5 to every agent)

- `kioku install <agent> [--project] [--no-instructions|--instructions]
  [--dry-run]` and `kioku uninstall <agent> [--project]`; `<agent>` ∈
  `claude-code | codex | cursor | gemini-cli | all`.
- Hook command = `<abs bin> hook <neutral> --agent <agent>` (Claude keeps M1's
  form without `--agent`). `<abs bin>` = `std::env::current_exe()`
  canonicalized; if it lies under a `target/` directory or a temp dir, warn
  that hooks will break when it moves (suggest `install.sh` / `~/.local/bin`).
- Ours = any handler whose `command` matches `is_kioku_command` (M1). Merge is
  idempotent: a second run is byte-identical; a moved binary replaces our entry
  in place; foreign handlers — even in the same group — are never touched.
- JSON files: parse with serde_json (`preserve_order`); invalid JSON or an
  unexpected type → leave untouched, print the snippet to add by hand. Files
  with comments (JSONC) are therefore not edited (Gemini/Cursor may tolerate
  comments; kioku does not rewrite them).
- Backup `<file>.kioku-bak` before the first modification of an existing file,
  never overwritten. Write via temp file + rename; keep existing mode; new files
  that hold the token 0600, others 0644. Parent dirs created as needed. A
  symlinked file (a dotfiles repository) is resolved first (`canonicalize`), so
  the link stays and its target is updated in place; the backup sits next to
  the target. A write that **adds** a bearer token (more `Bearer ` occurrences
  than before) to an existing group/world-readable file sets it to 0600 and
  prints one line saying so (never the token).
- **The token is never written inside a repository**: `--project` affects hooks
  and instructions only; MCP is always registered at user level. Print what
  changed, never the token.
- Project root for `--project` = `git rev-parse --show-toplevel` from cwd, else
  cwd (Claude keeps M1's cwd). Print a note that project hook files contain an
  absolute, machine-specific binary path and should not be committed.
  `install --project` refuses (error, nothing written) when that directory is
  the home directory itself (e.g. run from `~`, or `~` is a dotfiles git repo):
  the "project" files would be the user-level ones.
- Uninstall removes exactly our hook entries (empty event lists/objects left by
  that are removed), our MCP entry (Codex: the managed block) and our
  instruction block / `.mdc`. Foreign content stays. Like install, `uninstall
  --project` also removes the user-level MCP entry (M1 Claude behaviour).
  Uninstall never writes a backup: install backed up every pre-existing file
  before its first change, so a file without `<file>.kioku-bak` is one kioku
  created — such a file is deleted when nothing but `{}` / an empty
  `mcpServers` / blank text is left (Step 3). An instruction file that another
  agent also uses (Gemini's `context.fileName` = `AGENTS.md` makes Gemini and
  Codex share `<root>/AGENTS.md`) keeps its block while that other agent's hook
  file still holds kioku hooks; the last one uninstalled removes it.
- Claude Code's `settings.json` follows the same mode rule (new file 0644; M1
  created it 0600 — it holds no token).

### 8.2 Per-agent file table

| agent | hooks (user) | hooks (`--project`) | MCP (always user) | instructions |
|-------|--------------|---------------------|-------------------|--------------|
| claude-code | `~/.claude/settings.json` | `./.claude/settings.json` | `~/.claude.json` `mcpServers.kioku` (M1) | off by default |
| codex | `$CODEX_HOME/hooks.json` | `<root>/.codex/hooks.json` | `$CODEX_HOME/config.toml` managed block | `$CODEX_HOME/AGENTS.md` (or `.override.md`); project `<root>/AGENTS.md` |
| cursor | `~/.cursor/hooks.json` | `<root>/.cursor/hooks.json` | `~/.cursor/mcp.json` `mcpServers.kioku` | project only: `<root>/.cursor/rules/kioku.mdc` |
| gemini-cli | `~/.gemini/settings.json` `hooks` | `<root>/.gemini/settings.json` | `~/.gemini/settings.json` `mcpServers.kioku` | `~/.gemini/GEMINI.md`; project `<root>/GEMINI.md` |

### 8.3 `kioku install all`

Detects agents by directory existence under the home dir: `~/.claude`
(claude-code), `$CODEX_HOME` or `~/.codex` (codex), `~/.cursor` (cursor),
`~/.gemini` (gemini-cli). Installs each detected one (continuing past a failed
one), prints one line per agent (installed / unchanged / skipped: not detected /
error). `--agents codex,cursor` restricts the set; `--project` and
`--dry-run` pass through. Exit 1 if any selected agent errored. `kioku
uninstall all` removes from every agent whose files contain kioku entries,
detected or not.

## 9. Server and core changes

### 9.1 `GET /api/v1/sessions/{id}/context`

Returns the `SessionStartResponse` shape for an existing session without
side effects: `project_id`, `pending_handoff` = the handoff with `accepted_by
= {id}` (newest; `null` when the session accepted none), `state_excerpt`,
`recent_sessions` (as in `sessions/start`, but the session's own page is left
out — Stop may already have finalized it once). 404 for an unknown session.
Used by Cursor late context (§5.6). Core: `Store::session_context`.

### 9.2 `GET /api/v1/status` additions

`version` (server crate version), `index_schema_version` (on disk, from
`index/schema-version`; `null` if missing), `index_schema_expected`
(`INDEX_SCHEMA_VERSION`). Used by doctor. The fields are `#[serde(default)]`
in `StatusReport`, so a newer CLI still reads an M1 server's status (`version`
empty, `index_schema_version` null, `index_schema_expected` 0). The MCP
`kioku_status` text is unchanged.

### 9.3 Digest

- Edit-type entries accept `tool_input.file_paths` (array) in addition to
  `file_path` / `notebook_path`; each distinct path counts once per tool call
  (non-string / empty entries are ignored). The server-side sanitizer shrinks
  the longest string first, so the path list survives a 4 000-char patch.
- `errors` counts `tool_response.is_error == true` (already) — normalizers set
  it for Cursor failures and Gemini `error`.
- Session page / STATE labels use the stored agent label (`codex`, `cursor`,
  `gemini-cli`); no other change.

### 9.4 Sessions

`sessions/start` accepts `source = "implicit"` (§3.9) and `"fork"` (Codex);
`source` stays free text. No schema change. (M1 already stored any string, so
this needed tests only.)

## 10. Service management

### 10.1 `kioku serve --log-file <path>`

New flag: tracing output goes to `<path>` with size rotation (10 MiB, keep
`.1`–`.3`), implemented as a small `io::Write` behind `parking_lot::Mutex` (no
new crate; `kioku_cli::logfile`, file created 0600, no ANSI colours). Without the flag, stderr as in M1. Service definitions always pass
`--log-file <data_dir>/logs/serve.log`; stdout/stderr of the process go to
`<data_dir>/logs/serve.stderr.log` (panics, pre-logging failures).

### 10.2 Commands

```
kioku service install     # write definition, enable, start; idempotent (rewrite+restart only if content changed)
kioku service uninstall   # stop, disable, remove definition
kioku service start|stop  # start (launchd: a loaded job is restarted with kickstart -k) / stop
kioku service status      # installed?  loaded/active?  pid;  GET /api/v1/health result
kioku service logs [-f] [-n 200]   # tail serve.log (implemented in Rust, both platforms)
```

All refuse on a client-only machine (no `[server]` section) with a message.
All paths in definitions are absolute (no `~`, no `$HOME`); `<bin>` as in §8.1.

### 10.3 macOS: launchd LaunchAgent (no sudo)

`~/Library/LaunchAgents/dev.kioku.serve.plist` (mode 0644):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.kioku.serve</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/me/.local/bin/kioku</string>
    <string>serve</string>
    <string>--log-file</string><string>/Users/me/.kioku/logs/serve.log</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>KIOKU_DATA_DIR</key><string>/Users/me/.kioku</string>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>RUST_LOG</key><string>info,tantivy=warn</string>
  </dict>
  <key>WorkingDirectory</key><string>/Users/me/.kioku</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key><false/>
  </dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>/Users/me/.kioku/logs/serve.stderr.log</string>
  <key>StandardErrorPath</key><string>/Users/me/.kioku/logs/serve.stderr.log</string>
</dict>
</plist>
```

`PATH` lets `kioku serve` find Homebrew `git`. XML is rendered with escaping of
`& < > " '` in every string. Commands (`uid` from `id -u`):

| action | command |
|--------|---------|
| install (not loaded) / start (not loaded) | `launchctl bootstrap gui/<uid> <plist>` → `launchctl enable gui/<uid>/dev.kioku.serve` |
| install (plist changed while loaded) | `launchctl bootout gui/<uid>/dev.kioku.serve` → `bootstrap` → `enable` (a changed plist is only read at bootstrap) |
| restart; start while loaded; `kioku update`; setup on a version mismatch | `launchctl kickstart -k gui/<uid>/dev.kioku.serve` |
| stop / uninstall | `launchctl bootout gui/<uid>/dev.kioku.serve` (+ delete plist) |
| status | `launchctl print gui/<uid>/dev.kioku.serve` exit code = loaded; running = health check (the `print` text format is not a stable API — do not parse beyond `pid = N`, optional) |

`bootout` returns before launchd has torn the job down, so a `bootstrap` right
after it can fail (`Bootstrap failed: 5: Input/output error`): every bootstrap
is retried up to 5 times, 500 ms apart. `KeepAlive` = `SuccessfulExit: false`
restarts the server after a crash but not after a clean exit (e.g. a config
error it reported), and `ThrottleInterval` 10 s bounds a crash loop.

Note: `stop` via `bootout` also unloads; `start` bootstraps again. A stopped
LaunchAgent comes back at next login (RunAtLoad) unless uninstalled.

Idempotency (Step 4): `service install` rewrites the definition only when its
bytes differ (new file 0644, no backup — kioku owns it) and runs the
bootout → bootstrap → enable sequence only when it rewrote the plist or
`launchctl print` says the job is not loaded (the `bootout` only when it was
loaded); otherwise it runs nothing but the `id -u` / `print` queries.

### 10.3.1 LAN reachability on macOS (added 2026-09-28, v0.3.1)

Observed with v0.3.0 on two Macs with `bind = "0.0.0.0"`: LAN clients got
through the TCP handshake, but no request ever reached kioku. Loopback worked,
and a Python `http.server` on the same machine answered on the LAN. The cause
was **Little Snitch**, installed on both machines. It held the new, unknown
`kioku` binary's incoming connections, and its prompt showed only on the
server's own screen. Once kioku was allowed there, LAN clients got answers.
macOS Local Network privacy was suspected first but not shown to be involved.

What kioku does about it:

1. **Stable identity.** Firewalls and privacy prompts identify a program by its
   code signature. The linker's ad-hoc signature carries a per-build identifier
   (`kioku-<hash>`) and no bound Info.plist. Instead:
   - the macOS `kioku` binary embeds an `Info.plist` in `__TEXT,__info_plist`
     (kioku-cli `build.rs`, linker `-sectcreate`). It sets
     `CFBundleIdentifier` `dev.kioku.kioku`, `CFBundleName` `kioku`, the
     version keys, and `NSLocalNetworkUsageDescription` in English and
     Japanese;
   - the release workflow re-signs the binary ad hoc with
     `--identifier dev.kioku.kioku`, which binds the plist.

   Prompts and rule lists then show "kioku" (`dev.kioku.kioku`). An ad-hoc
   signature has no team identity, and it was observed not to survive every
   update: Little Snitch asked again for v0.4.0. So
   `scripts/sign-macos.sh`, the release workflow's macOS step, signs with a
   **Developer ID Application** certificate when the secrets exist, with
   hardened runtime, a secure timestamp and `--identifier dev.kioku.kioku`,
   and notarizes the zipped binary with `notarytool`. The secrets are:
   - `MACOS_CERT_P12` (base64 .p12) and `MACOS_CERT_PASSWORD`;
   - `NOTARY_KEY_P8` (base64 App Store Connect API key), `NOTARY_KEY_ID` and
     `NOTARY_ISSUER_ID`.

   A bare binary cannot be stapled, so Gatekeeper checks the ticket online;
   downloads made with curl carry no quarantine anyway. The team identity is
   what keeps a firewall rule valid across updates. Without the secrets the
   script falls back to the ad-hoc signature, and a missing notary key only
   skips notarization.
2. **Diagnosis.** On macOS, `kioku doctor` on a machine whose `[server] bind`
   is not loopback adds `server.lan`. It requests
   `http://<first non-loopback IPv4>:<port>/api/v1/health` with a 3 s timeout.
   It reports OK when the server answers. Otherwise it WARNs: "a firewall on
   this Mac blocks LAN clients from kioku", with the fix "allow incoming
   connections for kioku in your firewall (Little Snitch, LuLu, …) or in
   System Settings > Privacy & Security > Local Network, then restart the
   service".

kioku never edits firewall rules and never runs itself as root to get
around them; allowing it is the user's decision.

### 10.4 Linux: systemd user unit (no sudo)

`$XDG_CONFIG_HOME/systemd/user/kioku.service` (default
`~/.config/systemd/user/kioku.service`):

```ini
[Unit]
Description=kioku shared memory server for AI coding agents
After=network.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
ExecStart=/home/me/.local/bin/kioku serve --log-file /home/me/.kioku/logs/serve.log
Environment=KIOKU_DATA_DIR=/home/me/.kioku
Environment=RUST_LOG=info,tantivy=warn
WorkingDirectory=/home/me/.kioku
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Paths with spaces are quoted per systemd rules (`ExecStart=` arguments with
whitespace, quotes, `\` or `;` in double quotes; `Environment=` quoted as a whole
assignment; `%` → `%%` everywhere, `$` → `$$` in `ExecStart=`). Commands: `systemctl --user
daemon-reload`; `enable --now kioku.service`; `restart`; `stop`; `disable
--now`; status = `systemctl --user is-active kioku.service` (+ `show -p MainPID
--value`) + health check. `service install` (Step 4): unit bytes unchanged and
active → only `is-active` + the linger query run; changed → `daemon-reload`,
`enable --now`, and `restart` if it was active before; unchanged but inactive →
`enable --now`. `StartLimitIntervalSec=60` / `StartLimitBurst=5` stop a crash
loop (5 restarts within 60 s) instead of restarting forever.

**Linger**: a user unit stops at logout unless lingering is enabled. `service
install` runs `loginctl enable-linger` (no sudo; polkit usually allows it for
the own user) when `loginctl show-user $USER -p Linger` says `no`. If that
fails, print — do not run — the optional command `sudo loginctl enable-linger
<user>` ("only needed if kioku must run while you are logged out, e.g. on a
home server"). This is the only place kioku ever mentions sudo.

### 10.5 Fallback

No launchd and no working `systemctl --user` (`systemctl --user
show-environment` fails: WSL without systemd, containers, other OSes): `service
install` exits 1 with instructions: run `kioku serve --log-file …` under the
user's own supervisor, `nohup … &`, or the Docker image (`docker-compose`
example in the repo). `setup` treats this as a warning (§11).

## 11. `kioku setup`

```
kioku setup [--client-only <url> <token>] [--no-service] [--no-agents]
            [--agents a,b] [--bind <addr>] [--no-instructions] [--dry-run]
            [--print-client-command]
```

Non-interactive (safe under `curl … | sh`); every step idempotent. Order:

1. **Binary** — resolve `<bin>` (§8.1), warn if unstable location.
2. **Config** — `--client-only`: same as `kioku init --client-only` (writes
   `[client]`, verifies with `GET /api/v1/status`; failure → exit 1 before
   touching agents). Step 4: the check runs **before** the write, so a wrong
   token never replaces a working config (nothing at all is written on failure);
   its result is the `auth` line, and step 4 is not repeated. Otherwise: if `config.toml` is missing → `kioku init`
   (with `--bind` written to `[server] bind` when given); if present → keep
   (token never replaced); if it has only `[client]` → treat as client-only.
   **Former server machine** (added 2026-09-28): `--client-only <url>` on a
   config.toml that has a `[server]` section, where `<url>` is not this
   machine's own server (a loopback host on `[server] port`):
   - the file is copied to `config.toml.server-bak` (0600);
   - config.toml is rewritten with `[client]` only;
   - the summary adds `config  dropped this machine's [server] (backup: …)`;
   - step 3 removes this machine's installed kioku service
     (`service  removed this machine's <service> (now a client of <url>; data
     in <dir> kept)`).

   A leftover `[server]` made doctor treat the client as a server machine,
   and a leftover service kept serving stale data. The data dir is never
   touched. Found when moving a Mac's kioku to a Mac mini.
3. **Service** (full mode, unless `--no-service`) — if `GET <server_url>/api/v1/health`
   already answers and no kioku service is installed → "server already running
   (Docker/k3s?), not installing a service". If something non-kioku owns the
   port → fail. Else `service install`, then poll health every 200 ms for up to
   15 s. When the installed service answers but its health `version` differs
   from this binary's (the binary was replaced, e.g. by install.sh), it is
   restarted (`launchctl kickstart -k` / `systemctl --user restart`) and health
   is polled again until it reports this version; the line then reads
   `… running at <url>, restarted (v<old> -> v<new>)` (still another version
   after the restart → `!!`). Fallback platform (§10.5) → warning, continue.
4. **Auth check** — `GET /api/v1/status` with the token (401 → fail).
5. **Agents** (unless `--no-agents`) — `install all` semantics (§8.3).
6. **Summary** and exit code: 0 when steps 2–5 succeeded (warnings allowed),
   1 otherwise (agents are still installed when only the service failed —
   hooks fail open).

`--dry-run` prints the plan (files that would change, commands that would run)
and writes nothing (it still does read-only health / status requests; the auth
line is `--` when config.toml does not exist yet). `--print-client-command`
additionally prints the laptop command **including the token** (off by default):
`curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh -s -- --client-only http://<host>:7391 <token>`
(host = first non-loopback address when `bind` is `0.0.0.0`; with a loopback
bind the same address plus a note to set `bind = "0.0.0.0"`; on a client-only
machine its own `server_url`).

Summary format (ASCII only, one line per step; `ok` / `--` skipped / `!!`
warning / `xx` failed; `  <mark>  <step padded to 11> <text>`; the M2 draft's
example used `—`, replaced by `at` to stay ASCII, and gained the `auth` line):

```
kioku setup (v0.2.0)
  ok  binary      /Users/me/.local/bin/kioku
  ok  config      /Users/me/.kioku/config.toml (existing, token kept)
  ok  service     launchd dev.kioku.serve running at http://127.0.0.1:7391 (v0.2.0)
  ok  auth        token accepted by http://127.0.0.1:7391
  ok  claude-code hooks ~/.claude/settings.json (6), MCP ~/.claude.json
  ok  codex       hooks ~/.codex/hooks.json (6), MCP ~/.codex/config.toml, AGENTS.md
  !!  codex       open Codex and run /hooks once to trust kioku's hooks
  --  cursor      not detected (~/.cursor missing)
  ok  gemini-cli  hooks + MCP ~/.gemini/settings.json, GEMINI.md
Restart running agents so they pick up the new hooks and MCP server.
Check any time with: kioku doctor
```

Agent lines are the same whether the run installed or found everything in
place, so a second run prints the same summary (only the config line changes
from `(created, …)` to `(existing, token kept)`) and writes no file. The Codex
trust line is shown unless `[hooks.state]` records a trusted hash for a key
mentioning kioku (same heuristic as doctor). Installer `warning:` lines and
unparseable MCP files become `!!` lines; the snippet (which holds the token) is
left to `kioku install <agent>`. `--dry-run` prefixes agent lines with
`would change:` (or appends `(unchanged)`) and indents the installer's plan
under each line; the service line lists the file write and commands.
`--client-only` summary order: binary, config, auth, service (`--`), agents.

## 12. `kioku doctor`

```
kioku doctor [--json] [--agent <name>]
```

Each check prints `[ OK ]`, `[WARN]` or `[FAIL]` + one line (+ a `fix:` hint).
Exit 0 if no FAIL, 1 otherwise. `--json` → `{"checks":[{id, status:"ok"|"warn"|"fail", message, fix?}]}`.
Never prints the token. Runs with a 3 s per-request timeout.
Text form (Step 4): `[ OK ] <id>: <message>`, then `       fix: <hint>`, then one
count line. `auth`, `index` and `mcp` run only when the config is usable and
`server` found kioku (otherwise they would repeat the same failure); `index` and
`mcp` also need `auth` to pass. A detected agent with no kioku hooks at all is
`agent.<name>.hooks` WARN ("not installed"); its missing MCP entry is FAIL per
the table. `agent.cursor.duplicate` is always `[ OK ]` (the no-native-hooks case
is its info line). `--agent <name>` checks that agent even when not detected and
skips the others.

| id | check | OK | WARN | FAIL |
|----|-------|----|------|------|
| `binary` | current exe, version; `kioku` on PATH resolves to it | same file | not on PATH / PATH points elsewhere / unstable location | — |
| `config` | config.toml present, `[client]` with url + token; mode | 0600 | mode wider than 0600 | missing / no `[client]` / empty token |
| `data_dir` | (server machine) exists, 0700, wiki is a git repo | ok | perms wider / wiki not a git repo | missing |
| `git` | `git --version` | found | missing (wiki not versioned) | — |
| `server` | `GET /api/v1/health` | reachable, version equals client | version mismatch | unreachable |
| `auth` | `GET /api/v1/status` with token | 200 | — | 401 / other error |
| `index` | status `index_schema_version` vs `index_schema_expected` | equal | older/missing → "run `kioku reindex`" | — |
| `mcp` | `POST /mcp` initialize with token | server info returned | — | error |
| `service` | installed? active? (launchd / systemd) ; Linux linger | active | not installed on a server machine / linger off / fallback platform | installed but not running and health fails |
| `agent.<name>.hooks` | per detected agent: every expected event has our entry; the entry's binary path exists and is executable | all present | some events missing / points to a different kioku binary | path missing (moved binary) |
| `agent.<name>.mcp` | entry present; URL equals `[client] server_url`+`/mcp`; header token equals client token (compare, never print) | match | URL/token mismatch / present but file not parseable | missing |
| `agent.codex.feature` | `features.hooks` / `features.codex_hooks` not false; `codex --version` if on PATH (informational) | ok | — | disabled |
| `agent.codex.trust` | cannot be verified (§4.1) | — | always WARN "confirm in Codex /hooks" unless `[hooks.state]` shows a `trusted_hash` for an entry whose key mentions `kioku` (heuristic, UNVERIFIED key format) | — |
| `agent.gemini-cli.enabled` | `hooksConfig.enabled != false`; our names not in `hooksConfig.disabled` | ok | a kioku hook name is disabled | hooks disabled globally |
| `agent.cursor.duplicate` | Claude hooks installed and Cursor detected | native Cursor hooks present (sniff dedupes) | — (info line: Cursor also runs Claude hooks) | — |
| `agent.<name>.instructions` | snippet block present where expected | present | missing | — |
| `hook_log` | `hook.log` lines in the last 24 h | none | N errors, last one shown | — |
| `hook_dump` | dump enabled (env or config) | off | on ("raw payloads with possible secrets are being written") | — |

Doctor on a client-only machine skips `data_dir` and `service`.

## 13. `install.sh` and releases

### 13.1 Release assets (current `.github/workflows/release.yml`)

Tag `vX.Y.Z` → per target `kioku-vX.Y.Z-<target>.tar.gz` containing directory
`kioku-vX.Y.Z-<target>/` with `kioku`, `README.md`, `README.ja.md`,
`LICENSE-MIT`, `LICENSE-APACHE`; plus `kioku-vX.Y.Z-<target>.tar.gz.sha256`
(`shasum -a 256` output: `<hex>  <file>`). Targets today:
`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (cross),
`aarch64-apple-darwin`, `x86_64-apple-darwin`.

Required changes (Step 5):

1. Release job: after download, `cd dist && cat *.sha256 | sort -k2 > SHA256SUMS`
   and upload `SHA256SUMS` with the assets.
2. Add `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (built with
   `cross`): static binaries that run on any distro. Reason: the gnu builds come
   from `ubuntu-latest` and need its glibc, which older servers (e.g. Ubuntu
   22.04) do not have. Whether rusqlite `bundled` + lindera `embed-ipadic` build
   cleanly for musl under `cross` is **UNVERIFIED — check in CI**; if not, pin
   the gnu jobs to `ubuntu-22.04` instead and drop musl.
3. A CI job runs `shellcheck install.sh` and the install.sh tests (§16).

### 13.2 `install.sh` (POSIX sh, repo root, served from the raw GitHub URL)

```
curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh
curl -fsSL …/install.sh | sh -s -- --version v0.2.0 --no-setup
curl -fsSL …/install.sh | sh -s -- --client-only http://home.lan:7391 <token>
```

Options / env (flag wins over env):

| flag | env | default | meaning |
|------|-----|---------|---------|
| `--version <tag>` | `KIOKU_VERSION` | `latest` | release tag to install |
| `--install-dir <dir>` | `KIOKU_INSTALL_DIR` | `$HOME/.local/bin` | destination |
| `--repo <owner/name>` | `KIOKU_REPO` | `misorafa/kioku` (constant at the top of the script; `kioku_cli::setup::KIOKU_REPO` in Rust) | GitHub repository |
| `--from-source` | — | off | skip binaries, build with cargo |
| `--no-setup` | — | off | install only |
| everything after `--` or unknown `--client-only …` etc. | — | — | passed to `kioku setup` |
| (test only) | `KIOKU_DOWNLOAD_BASE`, `KIOKU_UNAME_S`, `KIOKU_UNAME_M` | — | override download base URL / uname for tests |

Behaviour:

1. `set -eu`; wrap the body in a `main` function called on the last line (safe
   against truncated downloads). Never calls `sudo`. Refuses to run as root
   unless `--install-dir` (or `KIOKU_INSTALL_DIR`) is given explicitly (a root
   install would put the service and config in root's home).
2. Needs `curl` or `wget`, `tar`, and `sha256sum` or `shasum -a 256`; missing
   → clear error.
3. Target: `uname -s` → `Linux` | `Darwin` (else → source fallback, or
   "unsupported" on Windows shells); `uname -m` → `x86_64|amd64` → `x86_64`,
   `arm64|aarch64` → `aarch64`. macOS: `<arch>-apple-darwin` (on Rosetta,
   `sysctl -n sysctl.proc_translated` = 1 → use `aarch64`). Linux: try
   `<arch>-unknown-linux-musl`, then `<arch>-unknown-linux-gnu`.
4. Tag: `latest` → follow `https://github.com/<repo>/releases/latest` and take
   the tag from the final URL (`…/releases/tag/<tag>`; no API call, no rate
   limit). No release (the lookup succeeds but the redirect does not land on
   `/releases/tag/`, e.g. on `/releases`) → source fallback. A failed lookup
   (curl/wget error: unreachable host, DNS, TLS, any HTTP error such as a 500
   from an outage or proxy) is **not** "no release": exit 1 with a network
   error (with `--from-source` only a warning; the default branch is built).
5. Download into `mktemp -d` (removed by `trap`):
   `…/releases/download/<tag>/kioku-<tag>-<target>.tar.gz` and `SHA256SUMS`
   (fallback: `<asset>.sha256` for releases that predate `SHA256SUMS`). Asset
   404 for this target → next candidate target → source fallback. Verify the
   checksum line for the asset name; mismatch or no checksum file → abort
   (never install unverified).
6. Extract; copy `kioku` to `<dir>/.kioku.new.$$`, `chmod 755`, run
   `<dir>/.kioku.new.$$ --version`; if it fails (e.g. glibc too old) remove it
   and try the next target, then source fallback. Only a binary that runs is
   `mv`ed over `<dir>/kioku` (atomic on one filesystem; a running service keeps
   the old inode). Step 5 change: the M2 draft renamed first and checked after,
   which could replace a working binary with one that does not run. These steps
   run inside `if try_target …`, where `set -e` is off, so every command there
   (`mkdir`, `cp`, `chmod`, checksum, `mv`, …) is checked explicitly and dies
   with a message; a directory at `<dir>/kioku` is refused (`mv` would move
   the binary into it).
7. Source fallback: needs `cargo` ≥ 1.91 (`cargo --version`) and `git`;
   otherwise print rustup instructions and exit 1. Inside a kioku checkout
   (`./Cargo.toml` with `kioku-cli`) → `cargo build --release --locked -p
   kioku-cli` and copy `target/release/kioku`; else `cargo install --locked
   --git https://github.com/<repo> [--tag <tag>] kioku-cli --root <tmp>` and copy
   `<tmp>/bin/kioku`. Warn that the first build takes several minutes (lindera
   dictionary download).
8. PATH: if `<dir>` is not in `$PATH`, print the exact line for the user's
   shell (`$SHELL` basename zsh/bash/fish) — never edit rc files.
   **Superseded by SPEC-M2.3 §4.2 / §8 (2026-09-28):** the line is now added to
   the shell's rc file by default (once, marked), `--no-modify-path` restores
   the print-only behaviour.
9. Unless `--no-setup`: `exec "<dir>/kioku" setup <passthrough args>` (absolute
   path, so PATH does not matter).

Output is plain ASCII lines prefixed `kioku-install:`. macOS: files fetched by
curl carry no quarantine attribute, so Gatekeeper does not block the binary
(re-check if a notarization requirement ever applies).

### 13.3 `kioku update` (optional in M2)

Implemented in Step 5 (`crates/kioku-cli/src/update.rs`; honours `KIOKU_REPO`
and `KIOKU_DOWNLOAD_BASE` like install.sh).

`kioku update [--version <tag>] [--check]`: same resolution/verification as
install.sh, implemented in Rust (`reqwest` + `sha2`; extraction by shelling out
to `tar`, as with `git`). The target triple is baked in at build time
(`build.rs` → `KIOKU_TARGET`), so a musl build updates to musl. Replace
`current_exe()` atomically (temp file in the same dir + rename); then, if a
kioku service is installed, restart it (`launchctl kickstart -k` / `systemctl
--user restart`); hook commands need no change (same path). Without
`--version` only a strictly newer release (semver compare of `X.Y.Z`) is
installed, otherwise "already up to date" — `latest` never downgrades a newer
local build. `--version <tag>` installs any other tag, including an older one. `--check` prints current vs latest and exits 0/10 (10 = update
available). If `current_exe()` is not writable (e.g. under `/usr/local/bin`
installed by root) → print the install.sh command instead; never sudo.

## 14. CLI summary (additions to M1 §11)

```
kioku setup [--client-only <url> <token>] [--no-service] [--no-agents] [--agents a,b] [--bind addr]
            [--no-instructions] [--dry-run] [--print-client-command]
kioku service install|uninstall|start|stop|status|logs [-f] [-n N]
kioku doctor [--json] [--agent <name>]
kioku install   claude-code|codex|cursor|gemini-cli|all [--project] [--no-instructions|--instructions]
                [--dry-run] [--agents a,b (all only)] [--enable-hooks-feature (codex)] [--trust-mcp (gemini-cli)]
kioku uninstall claude-code|codex|cursor|gemini-cli|all [--project] [--dry-run]
kioku hook <event> [--agent claude-code|codex|cursor|gemini-cli]
kioku hook-dump extract <agent> <event> [--out <dir>]
kioku serve [--bind] [--port] [--log-file <path>]
kioku update [--version <tag>] [--check]
```

New `[client]` keys: `hook_dump = false`, `cursor_late_context = true`.

## 15. Security notes

- Tokens only in user-level files (0600 when kioku creates them), never on argv,
  never in a repository, never printed except by `--print-client-command` and
  `kioku rotate-token --show-token` (SPEC-M2.7 §10). `kioku setup --client-only <url>`
  reads the token from `KIOKU_CLIENT_TOKEN` or one line on stdin; the old
  `--client-only <url> <token>` form still works but prints a deprecation warning.
  `kioku rotate-token` prints a `kioku invite` line (an invite created with the new
  token, valid 30 minutes) instead of the token. `kioku invite` and `kioku join` never
  print it.
- Memory is untrusted data (SPEC-M2.7 §3). Anything an agent wrote into kioku — pages,
  handoffs, the session summaries built from its observations — is exactly as trustworthy
  as the pages, files and web content that agent read while writing it: a prompt
  injection an agent picked up can be stored and replayed into every later session on
  every machine. kioku therefore introduces stored memory as data, not instructions (a
  fixed note at the top of the `<kioku>` block and of `kioku_read` / `kioku_query` /
  `kioku_handoff_pending`), keeps stored text from closing the `<kioku>` block, and
  redacts secrets in pages and handoffs as in observations. Agents must judge stored
  procedures before running them.
- kioku is single-user by design: one token, one person's machines. Everyone holding the
  token reads and writes all memory; there are no per-user permissions. One server = one
  person; the token is never shared between people. The machine name shown at session start
  (SPEC-M3.0 §6) tells that person's machines apart, not users.
- `hook-dump.jsonl` holds raw payloads: 0600, opt-in, flagged by doctor.
- Codex hook trust and Gemini project-hook fingerprints are the user's decision;
  kioku never writes trust state.
- install.sh verifies SHA-256 against the release before installing and never
  escalates privileges.

## 16. Tests

All tests use `tempfile` and a temp HOME (`HOME`, `CODEX_HOME`,
`XDG_CONFIG_HOME` pointed into it); no test touches the real home, launchctl or
systemctl.

1. **Fixtures** — `crates/kioku-cli/tests/fixtures/<agent>/<event>.docs.json`
   (built from §4.3/§5.3/§6.3) and later `<event>.captured.json` (from
   `hook-dump extract`; when both exist, tests run on both). Required set:
   - codex: session_start, user_prompt_submit, post_tool_use_bash,
     post_tool_use_apply_patch (patch touching 2 files incl. a Move),
     pre_compact, stop, stop_active, session_end;
   - cursor: session_start, before_submit_prompt, post_tool_use_shell,
     post_tool_use_read, post_tool_use_failure, after_file_edit, pre_compact,
     stop, stop_loop1, session_end, post_tool_use_empty_conversation_id,
     claude_import_stop (Cursor payload sent to a Claude-format hook);
   - gemini-cli: session_start, before_agent, after_tool_replace,
     after_tool_shell_error, after_agent, after_agent_active, pre_compress,
     session_end.
   At least one prompt fixture per agent is Japanese.
2. **Parsers** — per fixture: agent label, session id (Cursor fallback order),
   cwd resolution (payload → workspace_roots → env; Cursor: workspace_roots
   first; never process cwd for Cursor), `stop_hook_active` (Cursor `loop_count`), tool normalization table
   §3.5 (apply_patch paths extracted before truncation, from a > 4 000-char
   patch), `tool_output` string parsed, `is_error` set.
3. **Renderers** — golden `HookOutcome` for every cell of §3.6; Gemini outputs
   always parse as JSON (including error/fail-open paths); Codex Stop never
   prints to stdout on exit 0; Cursor never exits 2.
4. **Handlers against a live test server** (axum on an ephemeral port, as in
   M1): end-to-end per agent: start → prompt → 3 tool uses → stop nudges
   (codex exit 2; cursor followup JSON; gemini deny JSON) → handoff write →
   stop finalizes; implicit start on an unknown session (observation 404 →
   start → retry; prompt event returns the block); Cursor late context appears
   exactly once per session (marker), only on native `postToolUse`
   (`afterFileEdit` / `postToolUseFailure` leave the marker), and not when
   disabled; Cursor sniff: Claude-format invocation with Cursor payload is
   Silent when native Cursor hooks exist, re-dispatched otherwise; a
   Claude-shaped payload with `CURSOR_VERSION` in the env stays Claude Code;
   SessionEnd deadline cap for Codex; the real binary run with a non-UTF-8
   environment variable (name and value) exits 0 with JSON-only Gemini stdout.
5. **Hook dump** — writes one line per invocation incl. outcome, env filtered
   (no `KIOKU_AUTH_TOKEN`), mode 0600, rotation at 5 MiB, dump failure never
   changes the outcome; `hook-dump extract` round-trip.
6. **Installers** (per agent, user and `--project`): into a missing file;
   into a file with foreign entries (same event, same group for nested
   formats) — preserved; second run byte-identical; moved binary replaces in
   place; uninstall leaves exactly the foreign content; backup created once
   and never overwritten; invalid JSON untouched + snippet printed; token never
   in any `--project` file; new token-bearing files 0600.
   Codex TOML: append block to a file with comments and odd formatting — bytes
   outside the block identical; replace block; foreign `[mcp_servers.kioku]`
   untouched + warning; inline `mcp_servers.kioku = {…}` detected as foreign;
   unparseable file untouched; result re-parses; uninstall restores the
   original bytes exactly (modulo the backup); tables Codex (toml_edit) appended
   inside the block (`[projects."…"]`, `[hooks.state.x]`) survive
   install → install → uninstall.
   Symlinked config files are updated through the link (link kept); adding the
   token to a 0644 file makes it 0600 with one report line; `--project` from
   the home directory is refused; a shared `AGENTS.md` (Codex + Gemini via
   `context.fileName`) keeps its block until the last of the two is
   uninstalled.
   Instructions: block insert/replace/remove in an existing AGENTS.md /
   GEMINI.md; `.override.md` preference; `context.fileName` honoured; `.mdc`
   frontmatter exact.
   `install all`: detection matrix on temp HOME (none / some / all agent dirs),
   `--agents` filter, one failing agent does not stop the others.
7. **Service rendering** — golden plist and unit for a HOME with a space in
   it; all paths absolute; XML escaping; systemd quoting; command lists for
   install/start/stop/status generated through a recording runner (a struct
   with a `dry_run` flag that records argv instead of executing — not a
   trait); `service install` twice → second is a no-op; launchd start/restart of
   a loaded job = `kickstart -k`; a failing `bootstrap` after `bootout` is
   retried (5 attempts); on macOS CI, `plutil -lint` the rendered plist.
8. **Setup** — `--dry-run` on a temp HOME writes nothing; full run against a
   test server with `--no-service` installs agents and prints the summary;
   `--client-only` with a wrong token fails before touching agents; existing
   config token is kept; an installed, healthy service reporting another
   version is restarted once and the line reads `restarted (v<old> -> v<new>)`.
   `kioku update` without `--version` never installs an older or equal release.
9. **Doctor** — temp HOME: no config → FAIL `config`, exit 1; config + test
   server + all agents installed → all OK/expected WARN (codex trust), exit 0;
   moved binary → FAIL `agent.*.hooks`; token mismatch in an MCP entry → WARN,
   and the token text appears nowhere in stdout; Gemini `hooksConfig.enabled:
   false` → FAIL; Codex `features.hooks = false` → FAIL; `--json` schema.
10. **Server** — `GET /sessions/{id}/context` (404; returns the handoff this
    session accepted; no side effects), status version fields, digest with
    `file_paths`.
11. **install.sh** — `shellcheck`; a POSIX test script (run under `dash` and
    macOS `sh`) with `KIOKU_DOWNLOAD_BASE` pointing at a local fixture release
    (served by `python3 -m http.server`): correct install; checksum mismatch
    aborts and leaves no binary; missing `SHA256SUMS` uses `.sha256`; no
    checksum at all aborts; `--no-setup`; uname matrix via `KIOKU_UNAME_S/M`
    (Linux x86_64/aarch64 → musl then gnu, Darwin arm64/x86_64, FreeBSD →
    source fallback message); PATH hint printed when the dir is not on PATH;
    passthrough args reach `kioku setup` (fixture binary is a shell script that
    echoes its argv). Step 5: `scripts/test-install.sh`; the fixture server is a
    `python3` `http.server` subclass that also answers `releases/latest` with the
    GitHub-style redirect; a fake `id` and `cargo` on PATH cover the root refusal
    and the source fallback; `KIOKU_TEST_REAL_BIN=<path>` adds a run of the real
    binary's `setup --dry-run`. CI runs it under dash and macOS sh. A 500 on
    `releases/latest` and an unreachable host exit 1 with a network error and
    no source fallback (curl and wget); a directory at `<dir>/kioku` aborts.

## 17. Step plan (one agent run per step)

- **Step 1 — core + server**: §9 (context endpoint, status fields, digest
  `file_paths`, `implicit`/`fork` sources) with tests §16.10.
- **Step 2 — hook side**: Agent variants, parsers, normalization, renderers,
  Cursor sniff, implicit start, Cursor late context, per-agent deadlines,
  `KIOKU_HOOK_DUMP` + `hook-dump extract`, `stop_nudge_generic` strings.
  Tests §16.1–5.
- **Step 3 — installers**: codex / cursor / gemini-cli / all, instruction
  snippets, Codex TOML managed block. Tests §16.6.
- **Step 4 — machine setup**: `serve --log-file`, `service`, `setup`,
  `doctor`. Tests §16.7–9.
- **Step 5 — distribution**: `install.sh`, release.yml (`SHA256SUMS`, musl),
  CI job, optional `kioku update`. Tests §16.11.
- **Step 6 — verification by the planner with the real agents**: on one Mac
  and one Linux box run `install.sh`, then each agent with `KIOKU_HOOK_DUMP=1`;
  commit captured fixtures; resolve every item in §18 and update this spec.

## 18. UNVERIFIED items (resolve in Step 6 with captured payloads)

| # | item | where | fallback built in |
|---|------|-------|-------------------|
| 1 | ~~First Codex version with hooks on by default~~ — resolved 2026-09-27 with a real capture (npm Codex, `CODEX_MANAGED_BY_NPM=1`): hooks ran without any `[features]` entry, and all six registered events fired, including `Stop` | §4.1 | `--enable-hooks-feature` kept for older builds |
| 2 | Codex hook trust key format / hash input in `[hooks.state]` | §4.1, §12 | doctor always WARNs "confirm /hooks" |
| 3 | ~~Codex `tool_response` shape for Bash~~ — resolved: a plain string (stdout); `apply_patch` still UNVERIFIED. Captured extras per event: `model`, `permission_mode` (all), `turn_id` (all but SessionStart/End), `last_assistant_message` (Stop), `source: "resume"` on SessionStart. Fixtures: `tests/fixtures/codex/*.captured.json` | §4.3 | normalizer accepts any JSON |
| 4 | Cursor hook default timeout ("platform default") | §5.2 | kioku sets explicit timeouts |
| 5 | ~~Cursor common fields / `cwd`~~ — resolved 2026-09-27 (`cursor-agent -p` 2026.09.26): `conversation_id`, `session_id` (same value), `workspace_roots`, `cursor_version`, `user_email`, `model`, `generation_id` on sessionStart, postToolUse and sessionEnd; `cwd` only on postToolUse and there **empty** (`""`, also `tool_input.cwd`), so the root-first order of §3.4 is required; `transcript_path` is null on sessionStart; env adds `CURSOR_TRANSCRIPT_PATH`. That headless run fired **no** beforeSubmitPrompt or stop — sessionEnd (`reason`/`final_status: "completed"`) finalizes. Fixtures: `tests/fixtures/cursor/*.captured.json` | §5.3 | resolution order §3.4 |
| 6 | Cursor `Read` tool `tool_input` key for the path | §3.5 | tries `file_path`/`path`/`target_file`/`filePath` |
| 7 | Cursor sessionStart `additional_context` reaching the model (reported broken, U9) | §5.4 | late context §5.6 |
| 8 | Cursor `loop_count` scope (per turn vs per conversation) | §5.4 | at most one nudge per conversation if cumulative |
| 9 | ~~Payload Cursor sends to imported Claude Code hooks~~ — resolved: the native Cursor payload (camelCase `hook_event_name`, `conversation_id`), so the §3.7 sniff fires and, with native hooks installed, the Claude-side run is silent (verified live). MCP import from `~/.claude.json` still UNVERIFIED | §3.7, §5.5 | sniff accepts both shapes; duplicate MCP tolerated |
| 10 | Gemini `run_shell_command` `tool_response` text | §6.3 | digest uses command + `error` only |
| 11 | Gemini / Cursor tolerance of empty stdout (kioku prints `{}` anyway) — Cursor: resolved, the imported Claude hooks print nothing and the run completes normally; Gemini moot for personal accounts (SPEC-M2.1) | §3.6 | always print valid JSON |
| 12 | ~~musl cross-build of rusqlite(bundled)+lindera~~ — resolved 2026-09-27: the v0.3.0 release built all six targets (x86_64/aarch64 × linux-musl, linux-gnu, apple-darwin) | §13.1 | pin gnu builds to ubuntu-22.04 |
| 13 | ~~GitHub repo slug~~ — resolved in Step 4: `misorafa/kioku` | §13.2 | `KIOKU_REPO` / `--repo` still override it |
| 14 | ~~Does `KIOKU_HOOK_DUMP` reach Codex hooks?~~ — resolved: no. Codex passes only a core env (`CODEX_MANAGED_BY_NPM`, `CODEX_MANAGED_PACKAGE_ROOT` were the only extras seen), so the env switch never arrives; use `[client] hook_dump = true` (`scripts/probe-agents.sh` does this) | §3.8 | config key |

## 19. Connection robustness (added 2026-09-28, v0.3.2)

Field report (this Mac → a Mac mini server with `bind = "0.0.0.0"`):
`mini-M2.local` resolved to two IPv6 link-local addresses, `192.168.1.240` and
`192.168.1.57`. The server listened on IPv4 only and one IPv4 address did not
answer, so a hook tried dead addresses in turn and ran out of its 3 s. From
outside the LAN, over WireGuard, mDNS names do not resolve at all, and the
router's DNS does not know host names. DHCP reservations cannot be assumed.

### 19.1 Server: dual stack

With `[server] bind = "0.0.0.0"` on a unix platform whose IPv6 sockets are
dual stack by default, `kioku serve` listens on `[::]`, which accepts both IPv6
and IPv4 (v4-mapped) connections. That covers macOS, and Linux when
`/proc/sys/net/ipv6/bindv6only` reads `0`. If binding `[::]` fails (no IPv6),
it falls back to `0.0.0.0`. Any other bind value is used as given. The
"listening" log line shows the address actually bound.

### 19.2 Client: several addresses, and a last-good address

`ApiClient` (hooks, `search`, `status`, `reindex`, `doctor`):

1. **Connect timeout split across addresses.** The reqwest client gets
   `connect_timeout` = ⅔ of the invocation deadline, capped at 4 s. Without the
   cap, a 600 s command deadline let one dead address stall `kioku status` for
   4 s. hyper divides it across
   the addresses a name resolves to, and races IPv6 against IPv4 (Happy
   Eyeballs, 300 ms), so one dead address cannot eat the whole budget.
2. **Last-good addresses.** When `server_url` has a host *name* (not an IP
   literal), every successful response records the peer address
   (`Response::remote_addr`) in `~/.kioku/state/server-addrs.json`:
   `{"<host>:<port>": ["<ip>:<port>", …]}`, most recent first, at most 4, and
   written only when the list changes. A v4-mapped peer is stored as IPv4.
   If the peer is an IPv6 link-local address (`fe80::/10`), which only works
   on that LAN, the name's IPv4 addresses are resolved once and kept behind
   it, as long as the list holds no IPv4 yet, so the VPN case still has a
   routable address. This came up in the field: over the new dual-stack
   server, the first address learned was `fe80::…%14`.
3. **Try the last-good address first.** A request to a named host with a
   cached address first goes straight to those addresses: DNS is skipped via
   `resolve_to_addrs`, and the attempt gets ⅓ of the remaining time. If that
   attempt fails to connect, the request is retried once with normal
   resolution, which covers a server whose address changed.
   - Outside the LAN over a VPN that routes the LAN (WireGuard), the mDNS name
     no longer resolves, but a cached LAN address still works, because the
     VPN routes it.
   - At home, the cached address answers immediately and no mDNS lookup
     happens at all.

The cache is advisory: an unreadable or corrupt file is ignored. It holds
addresses only, never the token.

`kioku invite` (SPEC-M2.7 §4) advertises the address the route above picks; on a
machine with several (LAN, VPN such as Tailscale) it lists the others below the lines,
LAN first, and `kioku invite --host <addr>` prints the lines for any of them.

### 19.3 What this does not cover

Agents' MCP clients connect to the `server_url` written into each agent's
config (§8). They get neither the cache nor the retry. A machine that moves
between the LAN and a VPN should use a `server_url` that works in both
places: an IP address (routed by the VPN), a DNS name, or a VPN name such as
Tailscale MagicDNS. `kioku setup --print-client-command` keeps printing the
server's LAN IP for that reason, and now adds the `.local` name as an
alternative for machines that stay on the LAN. A local stdio MCP bridge
(`kioku mcp`), which would give MCP the same resilience, is a candidate for a
later version.

## 20. `kioku mcp`: a local stdio bridge for MCP (added 2026-09-28, v0.4.0)

Why: §19's connection robustness (last-good addresses, split connect timeout)
only helps kioku's own HTTP client. Agents' MCP clients connect to the fixed
URL in their config, so a machine that moves between the LAN and a VPN had
to pin an IP, which breaks when DHCP changes it. The token was also copied
into every agent's config file.

### 20.1 Command

`kioku mcp` runs an MCP server on stdin/stdout (rmcp stdio transport,
newline-delimited JSON-RPC). It serves the same six tools as the server's
`/mcp`, with the same names, descriptions, input schemas and `instructions`.
Each tool call is one REST request to `[client] server_url` through
`ApiClient` (§19.2), with a 30 s deadline per call. It keeps no MCP session
of its own with the server, so a server restart is invisible to the agent.

| tool | REST call | output |
|------|-----------|--------|
| `kioku_query` | `GET /search?q=&project=&scope=&limit=` (limit defaults to the MCP default, 8) | the `/mcp` hit format |
| `kioku_read` | `GET /pages/<path>` | the `/mcp` page format |
| `kioku_write_page` | `PUT /pages` | `wrote <path>` |
| `kioku_handoff_write` | `POST /handoffs` | `handoff recorded for <project_id>` |
| `kioku_handoff_pending` | `GET /handoffs/pending?project=&accept=` | the `/mcp` handoff format, or `none` |
| `kioku_status` | `GET /status` | the `/mcp` status format |

A server error comes back as the tool's error text: the server's `{error}`
message, which carries the same project hint as `/mcp`. A connection failure
reads `kioku server <url> unreachable: …`.

Additive REST fields, which an older server omits and which the bridge
tolerates missing:

- `POST /handoffs` returns `{id, project_id}`;
- `GET /status` adds `project_ids: [<id>…]` (`projects` is already the count).

### 20.2 Registration

`kioku install` / `setup` now register the bridge, not the URL, for every
agent:

| agent | file | entry |
|-------|------|-------|
| Claude Code | `~/.claude.json` | `mcpServers.kioku = {"type":"stdio","command":"<bin>","args":["mcp"]}` |
| Codex | `config.toml` managed block | `[mcp_servers.kioku]` `command = "<bin>"`, `args = ["mcp"]` |
| Cursor | `~/.cursor/mcp.json` | `mcpServers.kioku = {"command":"<bin>","args":["mcp"]}` |
| Gemini CLI | `~/.gemini/settings.json` | `mcpServers.kioku = {"command":"<bin>","args":["mcp"],"timeout":30000}` (+`trust`) |
| Antigravity | `~/.gemini/config/mcp_config.json` | `mcpServers.kioku = {"command":"<bin>","args":["mcp"]}` |

- No agent file holds the token or the URL any more. Both live only in
  `~/.kioku/config.toml`, so changing server is `kioku setup --client-only`
  plus an agent restart, and nothing else to edit.
- Re-running `install` / `setup` replaces an old URL-style entry in place.
- `--mcp-http` (install and setup) keeps the v0.3 URL + token form, for an
  agent machine that cannot run the kioku binary.

`kioku doctor`'s `agent.<a>.mcp` accepts either form:

- **stdio:** OK when the command is an existing executable and `args ==
  ["mcp"]`. It WARNs when the command is not this binary.
- **URL (v0.3):** checked as before, plus the hint "re-run `kioku install
  <a>` to switch to the stdio bridge".

### 20.3 What stays

The server keeps serving `/mcp` over streamable HTTP, for `--mcp-http`
agents and for clients without the binary.

## 21. `kioku rotate-token` (added 2026-09-28, v0.4.2)

Replaces the server's auth token, e.g. after it leaked into a log. Since
v0.4.0 (§20) agents hold no token, so the only places to update are the
server's config.toml and each client's config.toml.

`kioku rotate-token [--dry-run]`, run on the **server machine**:

1. **Refusals**, each with a message and nothing written:
   - config.toml has no `[server]` section: "run it on the server machine";
   - `KIOKU_AUTH_TOKEN` is set in the environment (Docker/k3s): env beats
     the file, so the message says to change the variable instead.
2. **New token.** A fresh `util::generate_token()` value becomes
   `[server] auth_token`. When `[client]` points at this machine's own
   server (a loopback host on `[server] port`), or its token equals the old
   server token, `[client] auth_token` gets the new token too. The file is
   written 0600 through `Config::save`, and its other keys are kept.
3. **Restart.** If kioku's service is installed it is restarted (launchd
   `kickstart -k` / `systemctl --user restart`). kioku then polls health and
   checks `GET /status` with the new token (up to 15 s). Without a service
   the message says to restart `kioku serve` by hand; `--dry-run` skips this.
4. **Output.** The old token is rejected from now on. It prints, on its own
   lines and marked as containing the token, the command for every other
   machine:
   `kioku setup --client-only <url> <new token>` (same URL rule as
   `--print-client-command`: the LAN IP, with the `.local` alternative).
   That command already verifies the token before writing (§11 step 2), and
   re-registers agents, so any `--mcp-http` URL entry picks up the new
   token.

Clients that are not updated get HTTP 401. Hooks stay fail-open and log it,
and `kioku doctor` shows `auth` FAIL with the fix "kioku setup --client-only".
