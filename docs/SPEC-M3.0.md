# kioku — SPEC-M3.0: what an agent sees at session start

Status: spec, 2026-10-01. Amends SPEC-M1 §7.2 (digest), §8.1 (Stop records nothing),
§8.3 (the `<kioku>` block), SPEC-M2 §3 (stop nudge). Read CLAUDE.md, SPEC-M1.md,
SPEC-M2.4.md (lanes), SPEC-M2.7.md §3 (memory is data) first. Source: the v0.8.0 audit.
Depends on M2.8 (cached digests, one commit per turn).

Problem: the block an agent receives is one handoff plus a STATE.md excerpt (session list
and file names). Decisions, open questions, saved pages and what the agent said last are
not visible at start, so the memory is only as good as the last handoff. The rule-based
digest cannot say what the agent concluded, so the automatic handoff says "次にやること:
不明". The Stop nudge fires on nearly every turn of a long conversation.

## 1. The `<kioku>` block (SPEC-M1 §8.3 replaced)

Order and caps (total cap `SESSION_START_CAP` 6,000 → 8,000 chars; each section has its
own cap and is truncated with `…(N more)`):

1. Header lines as today (project, session, lane, server) + the M2.7 untrusted-data note.
2. **引き継ぎ / Handoff** — as today (routed by lane; reference handoff on a branch).
3. **決定事項 / Decisions (carried)** — up to 15 lines: the `決定事項` items of the last 20
   handoffs of this project (all lanes), newest first, de-duplicated by normalised text
   (NFKC, trimmed, case-folded), each suffixed with its date `(09-28)`. Excludes items
   already in section 2.
4. **未解決 / Open questions (carried)** — up to 8 lines, same source and rules, excluding
   questions that a later handoff's `決定事項` resolves (exact normalised match of the
   question text, or a decision line that starts with the question's first 12 chars).
5. **ピン留め / Pinned pages** — pages with tag `pinned` in this project or `_global`,
   newest first, up to 3 pages, each: title, path and the first 400 chars of the body.
6. **最近のセッション / Recent sessions** — 5 entries from the server's `recent_sessions`
   (today unused by the CLI): `date agent [lane] @machine — title (path)`.
7. **最後の回答 / Last reply** — the last `assistant` observation of the previous session
   on this lane (§3), first 600 chars, when it exists and section 2 did not come from
   the same session's agent handoff.
8. Footer as today.

STATE.md gets the same sections 3–6 (so a human reading the wiki sees the same picture);
`state_excerpt` in the block is dropped in favour of the sections above.

Sections 3–5 are computed on the server in `start_session` (`SessionStartResponse` gains
`decisions: Vec<CarriedItem{text, date, handoff_id}>`, `open_questions: Vec<CarriedItem>`,
`pinned: Vec<PinnedPage{path, title, excerpt}>`, `last_reply: Option<String>`); older
clients ignore them, older servers omit them.

Tests: block golden test (ja and en) with handoffs across 3 sessions and 2 lanes, a pinned
page, and a duplicate decision; cap behaviour; a server response without the new fields
renders as before (compatibility).

## 2. Handoff schema additions

`HandoffInput` gains optional `gotchas: Vec<String>`（落とし穴・注意点）and
`verified: Vec<String>`（確認済みの事実）. Rendered in the handoff Markdown and carried like
decisions (section 3 shows `verified` lines after decisions, prefixed `✓`; `gotchas` go
to section 4 prefixed `⚠`, up to 5). MCP tool description updated (ja/en). Older servers
ignore unknown fields (serde default). Tests: round trip and rendering.

## 3. Capture the agent's last reply (SPEC-M1 §8.1 revised)

- Claude Code and Codex send `last_assistant_message` on Stop (real captures exist in
  `tests/fixtures/{windows/claude-code,codex}/stop.captured.json`). The Stop hook records
  it as an observation `kind = assistant` (payload `{text}`, sanitised, ≤ 2,000 chars)
  **before** finalize, only when non-empty and different from the previous `assistant`
  observation of the session. Cursor/Antigravity: when the payload has
  `transcript_path`/`transcriptPath`, read the last assistant entry (≤ 64 KiB tail read,
  deadline-bounded); otherwise nothing.
- The rules handoff gains a section 「最後の回答（要約）」: the first 400 chars of the last
  assistant observation; `auto_next_unknown` is used only when there is no last reply
  either.
- `kioku_query` indexes `assistant` observations? **No** — they stay in the session page
  (rendered under 「最後の回答」) which is indexed; raw observations are not indexed (M1).
- Tests: Stop fixture → assistant observation stored, sanitised; session page and rules
  handoff contain it; duplicate replies are not stored twice; Japanese reply in the
  fixture.

## 4. Stop nudge and finalize granularity (SPEC-M2 §3 revised)

- Nudge when **all** of: ≥ 3 tool uses since the last agent handoff (as today), **and**
  ≥ 10 minutes since that handoff (or no handoff yet and ≥ 10 minutes since session
  start), **and** not nudged in the last 10 minutes (`state/nudge-<session>` on the
  client). The nudge text additionally says the agent may answer the user first and write
  the handoff at the next natural pause.
- Finalize still runs on every Stop (it is cheap after M2.8 §1–2), but the rules handoff
  is (re)issued only when the digest changed in a way that matters: new prompt, new file
  edit, new commit, new assistant reply, or ≥ 5 new tool uses. Otherwise the previous
  rules handoff is updated in place (`update_handoff_content`, as today) without a new
  row.
- `[client] nudge_min_minutes = 10` and `nudge = true|false` in config.toml.
- Tests: `stop_decision` matrix with times; no new handoff row for a Stop with no
  meaningful change; the nudge text.

## 5. Search result lines show time and kind (audit PROD-4, part)

`format_hits` (MCP) and `kioku search` print `YYYY-MM-DD` and `kind` per hit:
`1. <path> — <title> (session, 2026-09-28) …`. Re-ranking itself is M3.1.

## 6. Machine identity (audit PROD-7)

`SessionStartRequest.machine` (hostname, ≤ 64 chars, sent by the client; env override
`KIOKU_MACHINE`), stored in `sessions.machine` (ADDED_COLUMNS, schema version +1).
Shown in section 6 (`@machine`), in session page frontmatter and in the handoff heading
(`claude-code@mini, 2026-10-01 10:12`). README/SPEC-M2 §15: "one server = one person; the
token is never shared between people".

## 7. Deliverables

Branch `m3.0-context`, draft PR against `main`, CI green. README / README.ja: the new
block layout with an example, `pinned` tag, `gotchas`/`verified`, nudge settings,
`machine`. Update SPEC-M1 §8.3 text to point here. Record deviations in §8. Do not merge,
tag or change secrets.
