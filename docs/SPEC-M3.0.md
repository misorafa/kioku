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

> Amended by SPEC-M3.1 §2 (`@machine` for sessions, re-ranking).

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

## 8. Implementation notes (2026-10-01, branch `m3.0-context`)

Where the text above was silent, impossible or (in hindsight) not the right thing, and what
was done instead. Old clients against this server and this client against an old server
both keep working: every new field has a serde default, and a response without
`context_version` renders the M1 §8.3 block.

1. **Detecting an older server (§1).** "Older servers omit them" cannot be told apart from
   "nothing to carry" with plain `Vec` fields, so `SessionStartResponse` also carries
   `context_version` (1). With 0 / absent the client renders the old layout (handoff +
   STATE excerpt, the old shrink order, now within the 8,000 cap). The server still sends
   `state_excerpt` so older clients keep their STATE excerpt.
2. **Where carried items come from (§1, §2).** They are read back from the stored handoff
   Markdown (`carry::handoff_items`, both languages, any heading depth), so the history
   written before this release is carried too — no new column. The source is the last 20
   **agent** handoffs (all lanes): rules handoffs have no item lists, and a rules addendum
   only repeats its agent handoff. To keep each item one line, `render_agent_handoff` now
   writes an item on one line (whitespace collapsed); before, a multi-line item became
   several lines.
3. **Separate lists for `verified` and `gotchas` (§1, §2).** The response has
   `verified: Vec<CarriedItem>` and `gotchas: Vec<CarriedItem>` next to `decisions` /
   `open_questions`. Decisions keep their 15 lines; verified facts get up to 5 after them
   (otherwise 15 decisions would always hide them); gotchas up to 5 after the 8 open
   questions. Section 2's own items are excluded from all four lists; its decisions also
   resolve open questions.
4. **Normalisation (§1).** NFKC through the lindera-analysis character filter already in
   the build (no new crate), whitespace collapsed, lowercased. The "first 12 chars" rule is
   applied to the normalised text. Dates are shown as `(MM-DD)`; the response carries the
   full `YYYY-MM-DD`.
5. **Section caps.** Decisions 1,200 chars, open questions 800, pinned 1,500, recent
   sessions 700, last reply 800 (headings included); whole lines (a pinned page is one
   item) are kept and the rest is counted as `…(N more)` / `…（ほか N 件）`; a single item
   larger than its cap is cut. The handoff gets everything left (at least ~2,400 chars
   when every section is full) and is shrunk at line boundaries as before. Stored text in
   every section is defanged like the handoff (SPEC-M2.7 §3).
6. **Pinned pages (§1 section 5).** Tag match is case-insensitive (`Pinned` works); order is
   the page's `updated` time, newest first (ties: newest written). STATE.md is rewritten at
   finalize, so a newly pinned page reaches STATE.md at the next finalize; the
   `<kioku>` block sees it immediately.
7. **Recent sessions (§1 section 6).** `RecentSession` gains `agent`, `lane`, `machine`
   (defaults). The session being started (a resumed one) is no longer listed in its own
   block. The path is the full wiki path, so it can be passed to `kioku_read`.
8. **Last reply (§1 section 7).** "The previous session on this lane" = the most recently
   started other session of the project on the same lane that has a prompt or tool use; its
   newest `assistant` observation, first 600 chars, as a Markdown quote. It is left out
   when the handoff in section 2 came from that session — of **any** source, not only an
   agent handoff, because a rules handoff now carries the reply itself (§3) and printing it
   twice wastes the budget.
9. **Capture (§3).** The reply comes from `last_assistant_message` (Claude Code, Codex) and
   also from Gemini CLI's `prompt_response` (AfterAgent, same cost: in the payload).
   Cursor / Antigravity: the last assistant entry in the last 64 KiB of
   `transcript_path` / `transcriptPath` (a JSONL line whose `role` / `type` /
   `message.role` is `assistant`, or an Antigravity `PLANNER_RESPONSE`; text from
   `content` / `text` / `message.content`, string or text parts), read on a helper thread
   given up after 500 ms. Those transcript formats are **not verified** against real
   Cursor / Antigravity transcripts with text replies (the captured Antigravity fixture has
   none). The observation is posted directly (never queued in the outbox); an older server
   answers 400 for the unknown kind, which the hook ignores silently. "Different from the
   previous assistant observation" is checked on the server (same text twice in a row →
   the earlier seq is returned, nothing stored), so the hook needs no state. An `assistant`
   observation reopens a finalized session like a prompt does. An empty text is refused
   (400). Retention stubs keep the reply text (it is what the digest reads).
10. **Rules handoff text (§3).** The excerpt is one line: `最後の回答（要約）: <first 400
    chars>` inside the auto-generated section, replacing the `次にやること: 不明` line (which
    stays when there is no reply). The session page quotes the full reply (≤ 2,000 chars)
    under `## 最後の回答`, so headings in a reply cannot start page sections.
11. **Nudge timing (§4).** The time since the last agent handoff (or the session start) is
    measured by the **server** (`SessionInfo.secs_since_handoff`), so client clocks do not
    matter; an older server does not report it and the time rule is then skipped (the
    count rule and the client throttle still apply). The throttle file
    `<kioku dir>/state/nudge-<session>` holds the unix time of the last nudge and is read
    only when every other condition says "nudge". `nudge_min_minutes` is used for both
    intervals; `nudge = false` and the existing `stop_nudge = false` / `KIOKU_STOP_NUDGE=0`
    each turn the nudge off. The nudge text also mentions verified facts / gotchas.
    The SessionStart hook reads no state file for this (the hard-rule allowance was not
    needed).
12. **Finalize granularity (§4).** A *pending* rules handoff of the session is refreshed in
    place on every finalize, as before. When the session's newest rules handoff was already
    accepted by another session, the observations since its `seq_at` are digested: a
    prompt, a file edit, a commit, a reply or ≥ 5 tool uses issue a new (pending) row;
    anything less refreshes the accepted row in place (no new row, nothing re-delivered).
    Its `seq_at` stays at the time it was issued, so small changes add up to a new row.
13. **Search lines (§5).** The score is no longer printed (`(kind, YYYY-MM-DD)` instead of
    `(1.23)`), as in the spec's example; the date is the page's `updated` date (UTC). A hit
    without a date prints its kind only.
14. **Machine (§6).** `KIOKU_MACHINE` verbatim (trimmed, control characters dropped, ≤ 64
    chars), else the host name up to its first dot (an IP address whole) from
    `COMPUTERNAME`, `HOSTNAME`, `/proc/sys/kernel/hostname`, `/etc/hostname`, and only then
    the `hostname` command (300 ms deadline; macOS has no file for it) — no new crate
    (`gethostname` would need `unsafe` or a new dependency). A resumed session keeps its
    machine when the new start sends none. Schema version 5 (`sessions.machine`). The rules
    handoff heading has no agent, so only agent handoffs show `agent@machine`.
15. **Not verified on real machines:** the Cursor and Antigravity transcript formats for
    replies (see 9), Gemini CLI's `prompt_response` on a real AfterAgent, the host name
    fallback on a Mac without `HOSTNAME` (the `hostname` command path), and how the new
    block reads to agents in long real sessions (cap pressure with many pinned pages).
