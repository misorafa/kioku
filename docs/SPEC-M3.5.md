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

## 5. Implementation notes

(filled in by the implementation)
