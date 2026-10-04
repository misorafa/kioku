# kioku — SPEC-M3.4: clients without hooks, and readable page names

Status: spec, 2026-10-04. Amends SPEC-M2.5 §3.3 (client update trigger), SPEC-M3.2 §2
(liveness), SPEC-M1 §4 / §8 (page paths, `kioku_write_page`). Read CLAUDE.md,
docs/INDEX.md, those sections and `crates/kioku-cli/src/{bridge,auto_update,liveness}.rs`
first. Source: two field findings of 2026-10-04 on the Windows machine.

## 1. The MCP bridge follows the server's version (audit of SPEC-M2.5 §3.3)

Finding: a machine that uses only desktop apps (Claude.app, Codex.app) never runs a
SessionStart hook, so the client auto-update of SPEC-M2.5 §3.3 never fires; the Windows
client stayed on 0.7.1 while the server moved to 0.9.3, with no `state/auto-update.json`
at all.

- `kioku mcp` (the stdio bridge every app starts) performs the same version check as the
  SessionStart hook: on its **first successful tool call** (not at process start — the
  app may start the bridge long before the server is reachable, and a failed call must
  not delay the app), it reads the server version from the `GET /api/v1/status` response
  it already needs for `kioku_status`, or from a one-off `status` call made in the
  background after the first call returns. It then runs
  `auto_update::after_session_start`'s decision (same `ClientFacts`, same
  `state/auto-update.json`, same 6 h per-target throttle, same `UpdateLock`, same
  winget / brew / not-writable → notice-only rules) and, when an update is due, spawns
  the detached `kioku update --version <tag> --background` through
  `auto_update::spawn_detached`. The bridge process itself keeps running the old binary
  until the app restarts it (Windows: the rename dance already handles the locked exe).
- The check runs at most once per bridge process and never on the tool call's critical
  path: it is a `tokio::spawn` after the first tool result has been written to stdout.
- The notice line (auto off / winget / brew / not writable) cannot be injected into a
  block here; instead it is appended once per day to the result text of the first
  `kioku_query` / `kioku_handoff_pending` call, prefixed `kioku:`, using the existing
  `last_notice` throttle.
- Liveness (SPEC-M3.2 §2): the bridge records `state/last-hook.json` entries under
  agent `mcp` with event `tool_call` after each successful call (one small write, as the
  hooks do), so `kioku doctor` and `kioku status --agents` show that app-only machines
  are alive.
- Tests: bridge test with a fake server whose `server_version` is newer → exactly one
  detached spawn recorded (observe via the state file, `KIOKU_DOWNLOAD_BASE` pointing at a
  fake release server with `allow_mirror`, as the e2e auto-update tests do); a second
  call does not spawn again; an older-or-equal server spawns nothing; winget path →
  notice appended once to the next query result; `last-hook.json` gets an `mcp` entry;
  a failing first call defers the check (no state file written).

## 2. Readable page names for non-ASCII titles (SPEC-M1 §4 extended)

Finding: `page_slug` keeps only ASCII alphanumerics and appends a 6-hex hash whenever it
dropped characters, so a Japanese-only title becomes `page-4565ee.md` and
「書き込みテスト2(…claude.ai…)」 becomes `2-claude-ai-4565ee.md`. This is deliberate
(file names must be identical on macOS / Windows / Linux / git / links: NFC/NFD, reserved
characters, URL encoding), and the hash prevents two titles collapsing onto one file. It
stays. What changes is that agents can give a readable name:

- `kioku_write_page` (MCP, HTTP `PUT /api/v1/pages`, stdio bridge) gains optional
  `slug: String`: ASCII `[a-z0-9]` words joined by single `-`, 1–64 chars, lowercased by
  the server; anything else → 400 / tool error naming the rule. When given, the page path
  is `<scope dir>/<slug>.md` (no hash). `slug` and `path` together → 400. Writing the
  same `slug` again replaces the page (as `title`/`path` do today); `expected_revision`
  applies unchanged.
- Without `slug`, the server **derives one from the title** when the title is readable
  after NFKC folding and transliteration is *not* attempted: if the ASCII part of the
  title is empty or shorter than 3 characters, the path is `<scope dir>/<YYYY-MM-DD>-<hash>.md`
  (date first, so a directory listing is at least chronological) instead of
  `page-<hash>.md`. Existing pages keep their paths (redirects are not needed: this only
  affects new pages).
- The MCP description of `kioku_write_page` (ja/en) says: 「タイトルが日本語だけのときは
  slug に英数字の短い名前（例: write-test-2）を付けると、wiki 上で見つけやすいパスになる」 /
  "for a non-ASCII title, give `slug` a short ASCII name so the file is findable in the
  wiki". `kioku_read` and `kioku_query` output are unchanged (title is already shown).
- `kioku doctor` is unchanged. README (en/ja): one paragraph under the MCP tools table.
- Tests: slug validation (valid / uppercase folded / invalid chars / too long / with
  path → 400), path for a Japanese-only title is date-prefixed, an ASCII title keeps the
  old slug, `slug` round-trips through MCP and the bridge with a Japanese title and is
  found by a Japanese `kioku_query`; existing-page paths are untouched (regression on a
  fixture page named `page-<hash>.md`).

## 3. Deliverables

Branch `m3.4-bridge-update-slug`, draft PR against `main`, CI green on every job. Update
docs/INDEX.md (client update trigger → M3.4 §1; page paths → M3.4 §2), SPEC-M2.5 §3.3 and
SPEC-M1 §4 get a "see M3.4" line, CHANGELOG Unreleased. Record deviations in §4. Do not
merge, tag or change secrets.

## 4. Implementation notes

Recorded with the implementation (branch `m3.4-bridge-update-slug`).

§1, the bridge:

- The decision is `auto_update::after_session_start` itself, called by the bridge with a
  `HookEnv` built from its environment; no logic is duplicated and the hook path is
  unchanged. The detached updater is started with `auto_update::spawn_background`
  (`spawn_detached`).
- "After the first tool result has been written to stdout": rmcp writes a tool's response
  after the handler returns and offers no hook after the write. The check is a
  `tokio::spawn` (then `spawn_blocking`) issued when the first successful result is handed
  back, so it can never delay that result, but it may start a few milliseconds before the
  bytes are flushed.
- `kioku_status` hands its own `version` to the check; any other first call makes the
  one-off `GET /status` in the background (`version` there, since `/health` has none —
  SPEC-M2.7 §11).
- At most once per process: an atomic flag is set by the first successful call. When the
  background `GET /status` itself fails (the server went away between the call and the
  check) the decision has not run, and the flag is re-armed for the next successful call.
- Only a bridge started as `kioku mcp` (config re-read from the environment) checks and
  records liveness; a bridge with a fixed config (in-process tests, embedding) does neither,
  so a test never starts an updater that would replace the test binary.
- The notice is taken from `after_session_start`, which records `last_notice` when it
  decides (as for the hook). If the app makes no further `kioku_query` /
  `kioku_handoff_pending` call in that bridge process, that day's notice is not shown.
  The appended text is `\n\nkioku: <notice line>`.
- Liveness: `liveness::record_named(path, "mcp", "tool_call", true, now)` after each
  successful call, on the same background task (serialized within the process). `kioku
  doctor` / `kioku status --agents` show `hooks.liveness.mcp` once an entry exists; it is
  always OK (an app may simply not have been used), with "(more than 7 days ago)" when old.

§2, page names:

- The page path rules are in SPEC-M1 §6.1, not §4 (§4 is project identity; its `slug()`
  for project ids is unchanged). The "Amended by SPEC-M3.4 §2" note is in §6.1, with a
  pointer in §4.
- A date-prefixed name changes every day, so "writing the same title again replaces the
  page" needed a lookup: a write by title alone (no `slug`, no `path`) first reuses the
  page that title already has in the scope directory — its pre-M3.4 name (`page_slug`:
  `page-<hash>.md`, `<base>-<hash>.md`) or any `YYYY-MM-DD-<hash>.md` — and only otherwise
  derives today's name. The date is UTC.
- "ASCII title keeps the old slug" takes precedence over "fewer than 3 ASCII characters":
  titles made only of ASCII words (`Go`, `x`, `Design Notes`) keep `page_slug`. The rule
  applies to titles that `page_slug` would hash.
- Titles with at least 3 ASCII characters after NFKC are `<NFKC ASCII slug>-<hash>`;
  without full-width characters this equals the old name. The hash is still over the
  (redacted, trimmed) original title, so it matches pre-M3.4 names.
- An empty `slug` (`""`, sent by MCP clients that fill every optional string) is absent;
  with a `slug`, an empty `path` is absent too. `slug` + non-empty `path` → 400 /
  tool error.
- The stdio bridge adds a line to the result when the written path does not end in
  `/<slug>.md` — a server older than M3.4 ignores the unknown field.
