# kioku (記憶)

[日本語](README.ja.md) | English

**Status: M1 — early, expect rough edges.**

kioku is a self-hosted memory server shared by all your AI coding agents on all
your machines. It is a single Rust binary. Everything it remembers is plain
Markdown in a git repository (the source of truth); SQLite holds metadata and a
tantivy index makes it searchable, with Japanese segmented properly by lindera
(IPADIC) — Japanese is the primary language, English works too. Agents reach it
through MCP (streamable HTTP) and lifecycle hooks: sessions are captured
automatically, and the next session — in another agent or on another machine —
starts with the previous session's handoff. By default it makes zero LLM calls:
summaries and handoffs are built by rules.

## The problem

- Every agent keeps its own memory (or none). What Claude Code learned on the
  desktop is unknown to the session on the laptop, and to the next tool you try.
- Continuing work means writing a handoff document by hand at the end of every
  session and pasting it into the next one.
- Most memory tools index text with tokenizers that cannot segment Japanese, so
  Japanese notes are effectively unsearchable.

kioku keeps one memory on a server you own, captures sessions through hooks,
and passes a handoff to the next session of the same project automatically.

## How it works

```
 any machine                                   kioku server (one per person)
+----------------------------------+          +---------------------------------------+
| Claude Code                      |          | kioku serve  (:7391)                  |
|  hooks --> kioku hook <event> ---+-- HTTP ->|  /api/v1  --+                         |
|  MCP client (kioku_* tools) -----+-- HTTP ->|  /mcp     --+--> kioku-core           |
+----------------------------------+  Bearer  |                 |- wiki/   Markdown + git|
                                       token  |                 |          (source of truth)
                                              |                 |- db/     SQLite metadata|
                                              |                 '- index/  tantivy + lindera|
                                              +---------------------------------------+
```

Session lifecycle (Claude Code):

```
SessionStart      kioku hook session-start --> POST /api/v1/sessions/start
                  stdout (added to the agent's context):
                    <kioku> project id, pending handoff, STATE.md excerpt </kioku>
UserPromptSubmit  \
PostToolUse        > sanitized observation --> POST /api/v1/observations
PreCompact        /
Stop              no handoff written yet and >= 3 tool calls?
                    yes -> exit 2 + nudge: "write a handoff with kioku_handoff_write"
                    no  -> finalize: session page + STATE.md (+ rule-based handoff)
SessionEnd        finalize (idempotent)
```

- **SessionStart injection**: the hook prints a `<kioku>` block with the project
  id, the pending handoff (if any) and an excerpt of the project's `STATE.md`.
- **Stop nudge**: if the agent used at least 3 tools and has not called
  `kioku_handoff_write`, the Stop hook exits with code 2 and asks it to record a
  summary, next steps, open questions and decisions. `stop_hook_active`
  prevents a loop. Disable with `[client] stop_nudge = false` or
  `KIOKU_STOP_NUDGE=0`.
- **Finalize** writes a session page, rewrites `STATE.md` and, when the agent
  wrote no handoff, generates one from rules (last instruction, files touched,
  commands, commits, error count). Every page write is a git commit.
- **Handoffs are single-use**: the next SessionStart of the same project
  consumes the newest pending handoff (older pending ones are marked
  superseded). `kioku_handoff_pending` with `accept=false` only peeks.

## Quickstart (one machine)

Requirements: Rust 1.95+ to build, `git` (optional; without it the wiki is not
versioned). The build downloads the IPADIC dictionary, so it needs network
access. Prebuilt binaries for Linux (x86_64, aarch64) and macOS (arm64,
x86_64) are attached to tagged GitHub releases.

```sh
cargo install --locked --path crates/kioku-cli   # installs `kioku`

kioku init                    # ~/.kioku: config.toml with a new token, wiki git repo, db, index
kioku serve                   # keep it running (127.0.0.1:7391)
kioku install claude-code     # in another terminal: hooks + MCP registration
```

`kioku install claude-code` merges hook entries into `~/.claude/settings.json`
(`--project` uses `./.claude/settings.json`; a `settings.json.kioku-bak` backup
is made before the first change) and runs `claude mcp add --transport http
kioku http://127.0.0.1:7391/mcp --header "Authorization: Bearer <token>"
--scope user`. If `claude` is not on PATH it prints the `~/.claude.json`
snippet instead. `kioku uninstall claude-code` removes exactly what it added.

Try it:

1. Open Claude Code in a git repository and give it a task. `kioku project id`
   prints the project id kioku uses for that directory.
2. When it finishes a turn after using a few tools, the Stop nudge asks it to
   call `kioku_handoff_write`.
3. Start a new session in the same repository (or `/clear`): the handoff is
   injected at SessionStart.
4. Search from the terminal:

```sh
kioku search 引き継ぎ
kioku search --project <id> --limit 5 設計 判断
kioku status
```

## Home server setup

Run one server for all your machines.

On the server:

```sh
kioku init
kioku serve --bind 0.0.0.0     # or [server] bind = "0.0.0.0" in config.toml, or KIOKU_BIND
```

On every other machine (laptop, desktop):

```sh
kioku init --client-only https://kioku.example.com <auth_token from the server's config.toml>
kioku install claude-code
```

`--client-only` writes only a `[client]` section and checks the URL and token
with an authenticated request.

**Never expose kioku to the internet without TLS and the token.** kioku
speaks plain HTTP and authenticates with one bearer token; put it behind one
of these:

- **Caddy** (automatic HTTPS) on the same host, with kioku bound to
  `127.0.0.1`:

  ```
  kioku.example.com {
      reverse_proxy 127.0.0.1:7391
  }
  ```

- **Cloudflare Tunnel** (`cloudflared`) pointing at `http://127.0.0.1:7391`
  — no open ports.
- **WireGuard** (or another VPN such as Tailscale): bind kioku to the VPN
  address and use `http://<vpn-ip>:7391` as the URL; the tunnel provides the
  encryption.

## Docker

```sh
docker build -t kioku .
docker run -d --name kioku -p 7391:7391 \
  -e KIOKU_AUTH_TOKEN="$(openssl rand -hex 32)" \
  -v kioku-data:/data kioku
```

The image runs `kioku serve` as a non-root user (uid 10001) with
`KIOKU_DATA_DIR=/data` and `KIOKU_BIND=0.0.0.0`. No `config.toml` is needed:
with `KIOKU_AUTH_TOKEN` set, the data directory is created on first start. Keep
the token — clients need it for `kioku init --client-only`. A bind-mounted host
directory must be writable by uid 10001. Extra arguments go to `kioku serve`
(e.g. `--port 8000`), and `docker exec kioku kioku status` works inside the
container.

With Compose (see `docker-compose.yml`), put `KIOKU_AUTH_TOKEN=…` in a `.env`
file next to it and run `docker compose up -d`. The same TLS advice applies:
the container speaks plain HTTP.

## MCP tools

| tool | input | what it does |
|------|-------|--------------|
| `kioku_query` | `query`, `project?`, `scope?` (`project`/`global`/`all`), `limit?` (default 8) | full-text search (Japanese and English); `project` narrows to that project plus global pages |
| `kioku_read` | `path` | reads a page by its wiki-relative path (as shown in query results) |
| `kioku_write_page` | `title`, `content`, `project?`, `scope?` (`project`/`global`), `tags?`, `path?` | saves a searchable Markdown page; the same title/path replaces it |
| `kioku_handoff_write` | `project`, `session?`, `summary`, `next_steps`, `open_questions`, `decisions` | records the handoff the next session of the project receives |
| `kioku_handoff_pending` | `project`, `accept?` (default false) | peeks at (or consumes) the pending handoff |
| `kioku_status` | — | counts, data dir and known project ids |

The server's MCP `instructions` tell the agent to query before exploring and to
write a handoff before stopping.

## Data layout

```
~/.kioku/                       # $KIOKU_DATA_DIR
  config.toml
  wiki/                         # git repository — source of truth
    _global/<slug>.md           # cross-project pages (scope = global)
    <project_id>/
      STATE.md                  # current state, rewritten at every finalize
      sessions/YYYY-MM-DD-<session>.md
      pages/<slug>.md           # pages written with kioku_write_page
  raw/<project_id>/<session_id>.jsonl   # append-only sanitized observations
  db/kioku.sqlite               # metadata, sessions, observations, handoffs
  index/tantivy/                # derived; `kioku reindex` rebuilds it from wiki/
  logs/hook.log                 # client-side hook failures
```

Pages are Markdown with YAML frontmatter; you can read and edit them with any
editor (run `kioku reindex` afterwards so search sees the change). kioku commits but never pushes: backing up (for example, pushing
`wiki/` to a private remote, and copying `db/`, where handoffs live) is up to
you.

## Configuration

`$KIOKU_DATA_DIR/config.toml` (default `~/.kioku/config.toml`):

```toml
[server]
bind = "127.0.0.1"      # 0.0.0.0 on a home server
port = 7391
auth_token = "…"        # generated by `kioku init`; serve refuses to start without one
# data_dir = "~/.kioku" # optional override of the data location
summary_lang = "ja"     # ja | en — session pages, STATE.md, generated handoffs

[client]                # used by `kioku hook`, `search`, `status`, `reindex`, `install`
server_url = "http://127.0.0.1:7391"
auth_token = "…"
timeout_ms = 3000       # hard deadline per hook
stop_nudge = true
lang = "ja"             # ja | en — SessionStart block and Stop nudge
```

Environment variables (env beats the file):

| variable | effect |
|----------|--------|
| `KIOKU_DATA_DIR` | data directory, also where `config.toml` is read from |
| `KIOKU_BIND` | `[server] bind` |
| `KIOKU_PORT` | `[server] port` |
| `KIOKU_AUTH_TOKEN` | both `[server]` and `[client]` `auth_token` |
| `KIOKU_SERVER_URL` | `[client] server_url` |
| `KIOKU_STOP_NUDGE` | `0` / `false` / `off` / `no` disables the Stop nudge |
| `RUST_LOG` | server log filter (default `info,tantivy=warn`) |

`kioku serve --bind <addr> --port <port>` overrides both.

## Project identity

A project id is computed from the working directory (`kioku project id [path]`
prints it):

1. `.kioku.toml` in the directory or any ancestor wins:

   ```toml
   project = "my-project"   # the id
   name = "マイプロジェクト"   # optional display name
   ```

2. In a git repository with an `origin` remote: `<repo>-<8 hex of
   sha256(normalized remote)>`, e.g. folder `proj` with remote
   `git@github.com:me/chord-life.git` → id `chord-life-…`, name `chord-life`.
   The remote is normalized (scheme, credentials, port and `.git` stripped,
   host lowercased), so an SSH clone on one machine and an HTTPS clone in a
   differently named folder on another share the same memory.
3. Otherwise (git without a remote, or no git): `<folder>-<8 hex of
   sha256(canonical path)>` — tied to that path on that machine.

Non-ASCII characters are dropped from the slug part (a Japanese-only name
becomes `proj`). Use `.kioku.toml` to merge or rename projects.

## Security notes

- **Auth**: one bearer token, one user. `kioku serve` refuses to start without
  a token, whatever the bind address. Every route except `GET /api/v1/health`
  requires `Authorization: Bearer <token>`, including `/mcp`, whose Host-header
  allowlist is disabled so the token is the guard. The default bind is
  `127.0.0.1`.
- `config.toml` holds the token in plain text; `chmod 600` it.
- **Sanitizer** — hook payloads are redacted on the client before they are
  sent (and again by the server):
  - AWS access key ids (`AKIA…`), `sk-…` keys, GitHub `ghp_…` / `gho_…`
    tokens, Slack `xoxb-` / `xoxa-` / `xoxp-` tokens, PEM private key blocks;
  - the value in `authorization|api_key|api-key|apikey|secret|password|token`
    followed by `:` or `=` (including `Bearer …`), and JSON values under keys
    with those names;
  - `tool_input` is truncated to 4 000 chars and `tool_response` to 2 000.
- **Not redacted**: anything that does not match those shapes — e.g. Google
  `AIza…` keys, Stripe `sk_live_…` keys, JWTs, passwords inside URLs
  (`postgres://user:pass@…`), personal data. Prompts, commands, file paths and
  the (truncated) output of `Read`/`Bash`/edit tools reach the server and end
  up in `raw/`, SQLite and, in digested form, the git history of `wiki/`.
  Transcripts are not uploaded.
- **Hooks are fail-open**: on any network or server error a hook logs one line
  to `logs/hook.log`, prints nothing and exits 0 within `timeout_ms`, so a
  down server never blocks your agent. The only non-zero exit is the
  deliberate Stop nudge (2).

## Comparison

Other projects give coding agents persistent memory — ai-memory, claude-mem
and memorix among them — and are worth a look. kioku's differences: Japanese
tokenization (lindera/IPADIC) so Japanese text is actually searchable,
Markdown-in-git as the source of truth, no LLM calls by default, and one
self-hosted server shared by every machine. On the roadmap: whole-life ingest
(mail, calendar) and bi-temporal facts.

## Roadmap

- **M1** (this release): server, Markdown/git store, Japanese search, MCP
  tools, Claude Code hooks and handoffs, rule-based summaries.
- **M2**: web UI + other agents' installers.
- **M3**: embeddings + bi-temporal facts.
- **M4**: ingest adapters.
- **M5**: eval harness.

## License

MIT OR Apache-2.0, at your option ([LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE)).
