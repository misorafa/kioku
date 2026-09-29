# kioku — SPEC-M2.4: handoff lanes per branch, and project aliases

Status: spec, 2026-09-29. Amends SPEC-M1 / SPEC-M2. Read CLAUDE.md, SPEC-M1.md
(§4 project identity, handoffs), SPEC-M2.md §3.9 and §11 first.

Two field problems, both found in daily multi-agent use:

1. **Parallel worktrees mix handoffs.** Orca (and plain `git worktree`) runs
   several agents on the same repository at once, one task per branch. They all
   resolve to the same project id (by design: the remote decides it). But a
   project has exactly one "pending handoff". So the handoff that worktree A's
   agent writes about task A is injected into the next session in worktree B,
   which works on task B, and accepting it there hides it from A.
2. **Adding a remote later changes the project id.** A repository without a
   remote gets a path-derived id (`<dir>-<hash(path)>`); once `origin` exists it
   gets a remote-derived id (`<repo>-<hash(remote)>`). Every earlier session,
   page and handoff stays under the old id. Found on kioku's own repository: the
   fix at the time was a hand-written `.kioku.toml`. Users must not need to know
   that.

## 1. Handoff lanes (problem 1)

### 1.1 Lane

A **lane** is the git branch a session works on, captured by the client at
session start: `git -C <root> symbolic-ref --short -q HEAD`. It is recorded
**only when it differs from the repository's default branch** (§1.2); on the
default branch, and in a detached HEAD or a non-git directory, the lane is
**none** (the project lane, where every handoff lives today).

- Why the branch and not the worktree path: a worktree is always on its own
  branch, the branch name means something to the user, and it is the same on
  every machine; paths are not.
- A lane name is used as-is (git branch names), trimmed, max 200 chars; longer
  names are truncated with a hash suffix.

### 1.2 Default branch

Resolved by the client, first hit wins:

1. `git symbolic-ref --short -q refs/remotes/origin/HEAD` → strip `origin/`;
2. `main` if `refs/heads/main` exists, else `master` if `refs/heads/master` exists;
3. unknown → lanes are never recorded for that repository (everything stays on the
   project lane, i.e. today's behaviour).

Git runs with the per-call hook deadline (quiet, CREATE_NO_WINDOW on Windows; one
`git` spawn helper).

### 1.3 Data

- `sessions.lane TEXT NULL`, `handoffs.lane TEXT NULL` (added columns, migration via
  the existing `ADDED_COLUMNS` mechanism; existing rows are NULL = project lane).
- `SessionStartRequest` gains optional `lane` (older clients omit it → NULL).
- A handoff inherits the lane of its session (agent handoffs through
  `kioku_handoff_write` with a `session`; rule handoffs from finalize). An agent
  handoff without a session goes to the project lane.

### 1.4 Which handoff a session gets

At session start (and `GET /sessions/{id}/context`, `kioku_handoff_pending` with a
`session`), for a session on lane L:

1. the newest **unaccepted** handoff of the project **on lane L** → injected and
   accepted (as today);
2. else, if L is not the project lane: the newest unaccepted handoff **on the project
   lane** is injected as **context only — not accepted**, under the heading
   「メインの引き継ぎ（参考）」 / "Main line handoff (for reference)", so the default
   branch keeps it;
3. else nothing.

A session on the project lane never receives another lane's handoff. Accepting on
lane L marks older unaccepted handoffs **of lane L only** as accepted.

`kioku_handoff_pending(project)` without a session keeps today's behaviour on the
project lane; with a new optional `lane` argument it reads that lane.

### 1.5 Visibility

- The `<kioku>` SessionStart block adds `lane: <branch>` when set.
- Session pages' frontmatter gets `lane:` when set; STATE.md "recent sessions" shows
  `[<lane>]` after the agent name.
- Search is unchanged (lanes never hide memory; they only route handoffs).

## 2. Project aliases (problem 2)

### 2.1 Rule

On `POST /sessions/start`, if the requested project id is **unknown** and there is an
existing project with **the same `root_path`** whose id is **path-derived** (it has
no `remote_url`) — i.e. this same checkout just gained a remote — then:

- the existing project is kept as the **canonical** project (its id, wiki directory,
  pages, sessions and handoffs do not move);
- its `remote_url` is set to the new remote;
- an **alias** `new id → canonical id` is recorded;
- the session is started under the canonical id, and the response's `project_id` is
  the canonical id (so the `<kioku>` block tells the agent the id to use).

A path-derived project is also matched when its `root_path` differs only by the
Windows `\\?\` prefix or a trailing separator.

### 2.2 Using aliases everywhere

- Table `project_aliases(alias TEXT PRIMARY KEY, project_id TEXT NOT NULL,
  created_at TEXT NOT NULL)`.
- Every place that accepts a project id from outside (session start, search `project`,
  `write_page`, handoffs write/pending, MCP tools) resolves an alias to its canonical
  id first. So another machine that clones the repository (and only ever computes the
  remote-derived id) lands in the same project.
- `.kioku.toml` still wins over everything (it is read by the client and sent as the
  id); no alias is created for a `.kioku.toml` id.
- `GET /api/v1/status` lists aliases (`aliases: [{alias, project_id}]`, additive).

### 2.3 Manual merge (for past splits)

`kioku project merge <from-id> <into-id>` (server machine or via API, bearer auth):
moves sessions, handoffs and pages of `from` into `into` (pages move to `into`'s wiki
directory with a git commit), records `from` as an alias of `into`, reindexes, and
prints what moved. `--dry-run` lists without changing. Refuses when either id is
unknown or they are the same. This lets the user fold the existing
`kioku-71002b89` project (created when kioku's own repo got its remote, before this
fix) into `ai-agents-shared-memory-02036d30`.

## 3. Tests

- Core: lane capture helpers (default branch rules, detached HEAD, no git) with a temp
  git repo; handoff routing matrix of §1.4 (same lane, other lane, project lane
  reference not accepted, accept scope per lane); migration from a DB without the new
  columns; alias creation on session start (same root, path-derived → alias; different
  root → new project; `.kioku.toml` id → no alias); alias resolution in search / pages /
  handoffs; `project merge` moves everything and is idempotent-safe; dry run.
- Server/MCP: `lane` in session start is optional (old client payload still works);
  `kioku_handoff_pending` with `lane`.
- CLI e2e: two worktrees of one repo (two branches) → handoffs stay in their lanes; the
  default-branch session gets the project lane; a feature-branch session with no own
  handoff sees the main handoff as reference and does not consume it.
- Japanese in any search-related test (CLAUDE.md rule 8).

## 4. Deliverables (for the implementing session)

Branch `m2.4-lanes-aliases`, draft PR against `main`, CI green on every job (ubuntu,
macOS, Windows, install scripts). README / README.ja: a short "parallel worktrees
(Orca)" note and the `project merge` command. Update this spec with anything that
turned out different. Do not merge, tag or change secrets.

## 5. Implementation notes (2026-09-29, branch `m2.4-lanes-aliases`)

Where the spec was silent or had to be made concrete. Each choice keeps
today's single-branch behaviour unchanged.

1. **§1.1 capture:** `kioku_core::project::lane(cwd, deadline)`. It runs
   `current_branch` first and stops on a detached HEAD, then `default_branch`.
   That is at most four `git` spawns per session start, each through
   `util::output_with_deadline` (the `quiet_command` helper, so CREATE_NO_WINDOW
   on Windows) with the hook's per-event deadline. It uses the hook's cwd, not
   the project root, so a `.kioku.toml` project inside git also gets lanes.
   `identify()`'s own git calls are unchanged. Truncation: 191 chars + `-` +
   8 hex = 200 (`normalize_lane`, also applied by the server).
2. **§1.3 resume:** a restarted session (resume / implicit start) takes the
   lane of its latest start: the branch may have changed. An agent handoff
   without `session` still attaches to the newest open session (M1), but it
   goes to the project lane as §1.3 says.
3. **§1.4 API shapes (additive):** `SessionStartResponse` and
   `GET /sessions/{id}/context` gain `lane` and `reference_handoff`. Both are
   omitted when unset, so an old client's payloads and replies are byte-for-byte
   what they were. `GET /handoffs/pending` gains `lane` (an empty value means
   the project lane). Without `lane`, the lane of `session` is used, else the
   project lane. The reply gains `reference_handoff`, which is never accepted,
   not even with `accept=true`. The MCP `kioku_handoff_pending` got both
   `session` and `lane`, because §1.4 routes by session. Its text for a
   reference starts with `no handoff on this lane. Main line handoff (for
   reference, not accepted)`.
4. **STATE.md (not in the spec):** every lane reads STATE.md, so its "latest
   handoff" is now the newest handoff **on the project lane**. Otherwise a
   branch's handoff would reach the default branch through the STATE excerpt,
   which §1.4 forbids. "Recent sessions" and "hot files" still cover all
   lanes. The `<kioku>` block drops STATE's latest-handoff section whenever a
   handoff or a reference is printed, since it is the same text.
5. **§1.5 block:** the lane line comes right after `session:`. Its text is
   localized (`strings.rs` `start_lane_line`). The reference heading is
   followed by one line explaining that it was not accepted.
6. **§2.1 "same checkout":** the server needs to tell a git id apart from a
   `.kioku.toml` id without changing the protocol. So an alias is made only
   when the requested id **equals `id_from_remote(basename(root), remote)`**.
   The existing project must have no `remote_url` and an id of the form
   `<slug>-<8 hex>`. Roots are compared after stripping `\\?\` and trailing
   `/` or `\`. If several projects match, the first by id wins. The canonical
   project's name, root and remote then follow the new identity, as every
   session start already did.
7. **§2.2 resolution** happens in `Store` (so the HTTP API and MCP share it):
   session start, `project()`, `write_page`, `write_handoff`,
   `pending_handoff*`, `latest_handoff` and `search` with a project scope.
   Status: `StatusReport.aliases` (`[{alias, project_id}]`), printed by
   `kioku status` and `kioku_status`.
8. **§2.3 merge:** new route `POST /api/v1/projects/merge {from, into,
   dry_run}`. It returns a `MergeReport` of counts plus `(old, new)` page
   paths. The CLI always goes through it, so it works from any client. The
   `from` project row is deleted, observations and raw logs move too, and
   aliases that pointed at `from` are re-pointed. `into` may itself be given
   as an alias. `from`'s STATE.md is dropped and `into`'s is rewritten after
   the reindex. A page name already taken in `into` becomes
   `<stem>-<from>[-N].md`. Running the merge again once `from` is an alias of
   `into` returns `already_merged: true` and changes nothing. An unknown `from`
   gives 404, and `from == into` gives 400.
