# kioku — SPEC-M3.5: handoff and session hygiene

Status: spec, 2026-11-20. Amends SPEC-M3.1 §1 (superseding), SPEC-M2.8 §3 (`[retention]`,
`kioku prune`), SPEC-M3.2 §1 (metrics, `kioku status`). Read CLAUDE.md, docs/INDEX.md,
those sections and `crates/kioku-core/src/{store.rs,db.rs,handoff.rs,carry.rs,store/maintenance.rs}`
first. Source: the operations audit of 2026-11-20 on the author's server (six weeks of
three machines on one 1.0.0 server; no errors, two findings below).

Findings:

- `kioku_handoffs_pending` was 214 while only ~40 lanes had any pending handoff. Pending
  handoffs pile up on **branch lanes worked by one long-lived session**: every Stop writes
  an agent handoff, the next SessionStart of the *same* session skips its own handoffs
  (SPEC-M3.1 §1 rule 2), and no other session ever starts on that lane, so nothing is
  accepted and nothing is superseded. One session had 38 handoffs, all pending; one lane
  (`ShinichiroGoto/DBの完全暗号化`) had 29 pending agent handoffs and 9 pending *rules*
  handoffs from a single session (the in-place refresh of the rules handoff at finalize,
  `store.rs` near `update_handoff_content`, did not apply for them).
- `kioku_sessions_open` was 16: 10 open sessions idle for more than a week, 4 of them with
  no observation at all (an agent opened and quit). They never get a digest or a session
  page, and they stay in "open" lists forever.

Neither is a correctness bug: peeks return the newest pending handoff of a lane, and the
carried decisions (SPEC-M3.0) read agent handoffs whatever their status. The fix is
hygiene, so history stays honest and the counters mean something.

## 1. A session's newer handoff supersedes its own older pending ones (SPEC-M3.1 §1 extended)

- After `write_handoff` inserts an agent handoff, and after the finalize path inserts a
  rules handoff, the **writer session's** older pending handoffs **of the same source**
  (`agent` or `rules`) on the same project and lane become `superseded` (`accepted_at =
  now`, `accepted_by = "superseded"`), in the same transaction / under the same lock.
  Other sessions' pending handoffs are untouched: acceptance keeps superseding those.
- Why this is safe: `newest_pending_handoff` already returns only the newest and never the
  writer's own; `carry` (SPEC-M3.0) reads `recent_agent_handoffs` regardless of status —
  add a test that pins this, so decisions carried from a now-superseded handoff still show;
  `history` shows them as `superseded`, which is what they are.
- Rules handoffs: find out why a session gets a second pending rules handoff instead of an
  in-place refresh (the observed case: one session, one lane, 9 rows). If the refresh
  condition is too narrow (for example it only matches the newest handoff of the lane, or
  only one that is still pending), widen it to "the session's own pending rules handoff on
  the lane" and record the cause in §5. §1 then only has to cover what refresh cannot.
- Not retroactive: the existing backlog is handled by §2.

## 2. Retention: stale pending handoffs and idle open sessions (SPEC-M2.8 §3 extended)

```toml
[retention]
handoffs_pending_days = 30   # pending handoffs older than this that are not the newest pending of their lane are superseded
sessions_idle_days = 7       # open sessions without an observation for this long are finalized; ones with no observation at all are deleted
```

- **Handoffs step.** For every (project, lane), the newest pending handoff (same
  `HANDOFF_ORDER` as a peek) stays pending whatever its age — it is the lane's current
  state. Every other pending handoff of that lane older than `handoffs_pending_days`
  becomes `superseded`. `0` turns the step off.
- **Sessions step.** An open session whose newest observation (or `started_at` when it
  has none) is older than `sessions_idle_days`:
  - with at least one observation → `finalize_session` (reason `idle`): digest, session
    page, STATE.md, rules handoff, exactly like a Stop finalize. A later observation
    reopens it as today (`add_observation`, "a prompt or tool use on a finalized session
    reopens it"), so nothing is lost if the agent comes back.
  - with no observation → the row is deleted (nothing else exists for it: no raw log,
    no page). The outbox replay of SPEC-M2.6 §3 recreates a session it still has
    observations for, so an offline client is not affected.
  - `0` turns the step off. SPEC-M3.1 §1 rule 3 (another session active within 30 min)
    is unaffected: an idle session is by definition outside that window.
- `PruneReport` gains `handoffs_superseded`, `sessions_finalized`, `sessions_deleted`
  (plain counts, like `observations_reduced`). `kioku prune` prints them; a dry run counts
  and changes nothing; the daily `retention applied` log line carries them.
- `kioku forget --session` keeps deleting handoffs the session authored (unchanged).

## 3. Metrics and `kioku status` (SPEC-M3.2 §1 extended)

- `GET /api/v1/metrics` adds `kioku_handoffs_pending_lanes` (lanes with at least one
  pending handoff) and `kioku_sessions_idle` (open sessions past `sessions_idle_days`).
  Existing gauges keep their meaning.
- `kioku status` (and `--watch`) shows `handoffs pending: N on M lanes` and
  `sessions open: N (idle: K)`; against an older server the new numbers print `?`, like
  the other metric lines.

## 4. Deliverables

Branch `m3.5-hygiene`, draft PR against `main`, CI green (fmt, clippy `-D warnings`, all
tests). Do not merge, tag or change secrets. Update: `docs/INDEX.md` (handoff consumption,
retention and metrics rows point here too), `CHANGELOG.md` (Unreleased), README and
README.ja (`[retention]` snippet and the `kioku prune` paragraph), the config reference if
one lists `[retention]` keys. Tests (next to the code, `tempfile`, Japanese throughout —
use the lane name `ShinichiroGoto/DBの完全暗号化`):

- §1: a session writing three agent handoffs on a lane leaves one pending and two
  superseded; a second session's pending handoff on the same lane is untouched; rules
  handoff refresh vs insert; `carry` still carries a decision from a superseded handoff;
  `history` shows the statuses.
- §2: prune on a fixture with old/new pending handoffs on two lanes and with an active, an
  idle, and an empty open session — dry run changes nothing and reports the counts; the
  real run supersedes only the old non-newest handoffs, finalizes the idle session (a
  session page exists afterwards), deletes the empty one, leaves the active one; a later
  observation reopens the finalized session; `0` disables each step.
- §3: metric lines present and correct on the same fixture; `kioku status` rendering.

Record deviations and causes found in §5.

## 5. Implementation notes (2026-11-20, branch `m3.5-hygiene`)

### Why one session had 9 pending rules handoffs on one lane (§1)

The refresh condition of the finalize path was too narrow, and not in the ways guessed in
§1. It looked for the session's newest *pending* rules handoff (any lane), but then dropped
it when it was **older than the session's newest agent handoff**: an addendum (SPEC-M1
§7.1 step 4, "work continued after the agent's handoff") had to rank above the agent
handoff it extends, and refreshing the old row in place keeps its `created_at`, so the code
inserted a new row instead. On a long-lived branch session the cycle is: Stop → rules
handoff R1; the agent writes A1; ≥ `HANDOFF_STALE_TOOL_USES` (3) tool uses; Stop → R1 is
older than A1, so R2 is inserted; the agent writes A2; … — one new pending rules row per
agent handoff followed by real work, none ever superseded (nobody else starts on the lane,
and the session never accepts its own). 29 agent handoffs and 9 rules rows on
`ShinichiroGoto/DBの完全暗号化` is that cycle.

Fix (`store.rs`, step 4 of finalize): the row to refresh is now **the session's own
pending rules handoff on the session's current lane** (`db::own_pending_handoff`). When it
predates the session's newest agent handoff it is *reissued* above it (`db::reissue_handoff`:
content, `created_at`, `seq_at` replaced) instead of a second row being inserted; otherwise
it is refreshed in place with `created_at` kept, as before. An already accepted rules
handoff is still refreshed in place only when nothing meaningful changed (SPEC-M3.0 §4);
that lookup is now also restricted to the session's lane (a session whose branch changed
used to have its old lane's row rewritten). A rules row is inserted only when the session
has no pending one on the lane, and the insert then supersedes the session's other pending
rules handoffs on the lane (§1) — in practice rows left by an older server.

### Superseding own handoffs (§1)

- `db::supersede_own_pending` (same session, source, project and lane; `accepted_by =
  "superseded"`, `accepted_at = now`). `write_handoff` runs insert + supersede in one
  transaction under the DB lock; finalize runs them under the same DB lock.
- A handoff written without a session (`session` omitted and no open session) supersedes
  nothing. When `session` is omitted but an open session is found, the handoff goes to the
  project lane (SPEC-M2.4 §1.3), so it supersedes that session's older pending handoffs on
  the project lane only.
- A session's pending rules handoff is *not* superseded by its newer agent handoff (other
  source, as specified): a session has at most one pending rules row per lane, and it ranks
  below the agent handoff until it becomes the addendum. §2 cleans it up if it gets old.

### Retention (§2)

- **Order:** the sessions step runs before the handoffs step, so a rules handoff issued
  for an idle session takes part in the handoffs step like any other.
- **Deviation — idle finalize is dated by the last activity.** A finalize issued by
  retention uses the session's newest observation `ts` (its `started_at` without one) as
  `ended_at` and as the `created_at` of a new rules handoff, not "now". With "now", a
  session that went quiet ten days ago would issue a rules handoff that outranks every
  handoff written on the lane since (peeks and SessionStart take the newest), which is
  wrong. Everything else is a Stop finalize (`Store::finalize_as_of`, called by
  `finalize_session` with `None`). Re-finalizing on a later Stop uses "now" as before.
- **"No observation" sessions are deleted only when they also authored no handoff**
  (`write_handoff` can name a session that has no observation). Such a session is
  finalized instead (a non-substantive finalize: status only, no page), so the handoff
  keeps its author. Deletion is one `DELETE … WHERE status = 'open' AND NOT EXISTS
  (observations) AND NOT EXISTS (handoffs)`, so an observation that arrives meanwhile
  wins. The idle list is re-read right before each finalize for the same reason.
- A failing finalize of one idle session is logged (`finalizing an idle session failed`)
  and the run continues, so one broken session cannot stop the daily prune.
- A dry run counts the idle sessions it would finalize / delete and the handoffs it would
  supersede on the current state (it does not simulate the rules handoffs the sessions
  step would issue).
- The finalize log line is `session finalized` with `reason = "idle"`; the daily `retention
  applied` line carries `handoffs_superseded`, `sessions_finalized`, `sessions_deleted`.
- New `PruneReport` fields are `#[serde(default)]`, so a newer client reads an older
  server's report (as 0).

### Metrics and status (§3)

- `kioku_handoffs_pending_lanes` counts distinct `(project_id, lane)` pairs with a pending
  handoff; `kioku_sessions_idle` uses the same query as the sessions step (0 when
  `sessions_idle_days = 0`).
- Wording: plain `kioku status` changes its `sessions` and `handoffs` lines to
  `sessions     : 120 (open: 16, idle: 10)` and `handoffs     : 300 (pending: 214 on 40
  lanes)` (the column layout of that command); `--watch` shows `sessions open : 16 (idle:
  10) of 120` and `handoffs      : 214 pending on 40 lanes`. Plain `status` now also reads
  `/metrics`; any number the server does not report prints `?`.

### Tests

`store::handoff_tests::{a_sessions_newer_handoff_supersedes_its_own_older_pending_ones,
rules_handoff_is_refreshed_not_duplicated_on_a_long_lived_lane,
inserted_rules_handoff_supersedes_the_sessions_older_pending_ones}`,
`store::maintenance::tests::{prune_supersedes_stale_handoffs_and_settles_idle_sessions,
zero_days_disable_the_hygiene_steps}` (also the §3 gauges on the same fixture),
`config::tests::retention_table_and_backup_keep_fallback` (new defaults),
`kioku-server` `metrics::tests::renders_every_family_and_escapes_labels` and
`tests/metrics.rs` (gauges present), `kioku-cli`
`commands::tests::{status_shows_pending_lanes_and_idle_sessions,
prune_report_lists_hygiene_counts}`. The store tests use the lane
`ShinichiroGoto/DBの完全暗号化` and Japanese prompts, summaries and decisions.
