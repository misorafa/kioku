# kioku (記憶)

[日本語](README.ja.md) | English

**Status: under active development (v0.x, M2.1).** It works day to day on the
author's machines, but expect rough edges, and config formats and APIs may still
change between minor versions.

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
                    <kioku> project id, session id, pending handoff, STATE.md excerpt </kioku>
UserPromptSubmit  \
PostToolUse        > sanitized observation --> POST /api/v1/observations
PreCompact        /
Stop              >= 3 tool calls since the last handoff (or since start)?
                    yes -> exit 2 + nudge: "write a handoff with kioku_handoff_write"
                    no  -> finalize: session page + STATE.md (+ rule-based handoff)
SessionEnd        finalize (idempotent)
```

- **SessionStart injection**: the hook prints a `<kioku>` block with the project
  id and the session id (both to be passed to `kioku_handoff_write`), the
  pending handoff (if any) and an excerpt of the project's `STATE.md`.
- **Stop nudge**: if the agent used at least 3 tools since its last
  `kioku_handoff_write` in this session (or since the session started, if it
  wrote none), the Stop hook exits with code 2 and asks it to record a summary,
  next steps, open questions and decisions. `stop_hook_active` prevents a loop.
  Disable with `[client] stop_nudge = false` or `KIOKU_STOP_NUDGE=0`.
- **Finalize** writes a session page, rewrites `STATE.md` and, when the agent
  wrote no handoff, generates one from rules (last instruction, files touched,
  commands, commits, error count). When the agent's handoff is stale (3+ tool
  calls after it), finalize appends a rule-based addendum for that later work
  (「引き継ぎ（自動生成・追記）」) and hands over both. Every page write is a git
  commit.
- **Handoffs are single-use**: the next SessionStart of the same project
  consumes the newest pending handoff (older pending ones are marked
  superseded). `kioku_handoff_pending` with `accept=false` only peeks.

## Install

One line, no sudo (macOS and Linux, x86_64 and arm64):

```sh
curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh
```

`install.sh` downloads the release binary for your machine (Linux: the static
musl build first, then the glibc one), verifies it against the release's
`SHA256SUMS` (it refuses to install anything it cannot verify), installs it to
`~/.local/bin/kioku` (atomic rename) and runs `kioku setup` (next section). If
`~/.local/bin` is not on your `PATH` it adds one marked line,
`export PATH="$HOME/.local/bin:$PATH" # added by the kioku installer`, to your
shell's rc file (`~/.zshrc`; bash: `~/.bashrc`, on macOS `~/.bash_profile`;
fish: `~/.config/fish/conf.d/kioku.fish`; otherwise `~/.profile`), once, so the
`kioku` command works in new terminals. `--no-modify-path` only prints the line
instead. Options:

| option | env | default | |
|--------|-----|---------|-|
| `--version <tag>` | `KIOKU_VERSION` | `latest` | release to install |
| `--install-dir <dir>` | `KIOKU_INSTALL_DIR` | `~/.local/bin` | destination |
| `--repo <owner/name>` | `KIOKU_REPO` | `misorafa/kioku` | GitHub repository |
| `--join <url> <code>` | `KIOKU_JOIN_URL`, `KIOKU_JOIN_CODE` | | run `kioku join` (see below) instead of `kioku setup` |
| `--no-modify-path` | | | do not touch shell rc files |
| `--from-source` | | | build with cargo instead of downloading |
| `--no-setup` | | | install the binary only |

Every other argument (and everything after `--`) is passed to `kioku setup`:
`curl -fsSL …/install.sh | sh -s -- --version v0.2.0 --no-setup`. It refuses to
run as root unless `--install-dir` is given (kioku is a per-user install).
Without a prebuilt binary for your platform it builds from source, which needs
Rust 1.91+ (`rustup update` if older) and `git`; the first build takes several
minutes (it downloads the IPADIC dictionary). If GitHub cannot be reached
(network or HTTP error) it stops with an error instead of falling back to a
source build. By hand:
`cargo install --locked --git https://github.com/misorafa/kioku kioku-cli`, or
`cargo install --locked --path crates/kioku-cli` in a checkout. `git` is
optional at runtime (without it the wiki is not versioned).

### Adding another machine: `kioku invite`

Other machines (laptop, desktop, Windows PC) talk to one server. To add one, run
this **on the server**:

```
$ kioku invite
追加するマシンで、次のどちらか 1 行を貼り付けてください（10 分間・1 回だけ有効）:
Paste ONE of these on the machine to add (valid 10 minutes, once):

  Windows (PowerShell):  $env:KIOKU_JOIN='192.168.1.240:7391/K7Q2M9XD'; irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1 | iex
  macOS / Linux / Git Bash:  curl -sSL http://192.168.1.240:7391/i/K7Q2M9XD | sh

(On this LAN you can also use http://mini-M2.local:7391/…; over a VPN use the IP.)
```

and paste the matching line on the new machine. That one line installs kioku
(verified download, as above), puts it on `PATH`, fetches the server's token
with the one-time code (the token is never shown or copied), writes a
client-only `config.toml`, sets up every detected agent and ends with
"kioku の準備ができました … / kioku is ready - restart Claude Code, …". A line
that is expired or already used fails with one sentence saying to run
`kioku invite` again. `--ttl <minutes>` (up to 60) and `--uses <n>` (up to 20)
make one line work for several machines. On the new machine the same step is
`kioku join <url> <code>` if kioku is already installed. After
`kioku rotate-token`, run `kioku invite --uses <n>` and paste the new line on
each machine: `join` replaces the old client config.

The manual alternative still works: `kioku setup --print-client-command` on the
server prints a command with the token in it,

```sh
curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh -s -- --client-only http://<server>:7391 <token>
```

Update later with `kioku update` (same download and checksum verification;
replaces the binary in place and restarts the service; it only installs a
release newer than the running one — `--version <tag>` installs any tag,
including an older one; `kioku update --check` exits 10 when a newer release
exists), or by re-running the one-liner (the next `kioku setup` restarts a
service still running the old version).

On macOS, never overwrite the installed binary with `cp` onto the existing file:
the kernel caches the old code signature and kills the new binary (SIGKILL,
"zsh: killed"). Remove it first — `rm ~/.cargo/bin/kioku && cp target/release/kioku ~/.cargo/bin/` —
then `kioku service stop && kioku service start`. `kioku update` and
`cargo install` already replace the file this way.

### Windows (client only)

On Windows 11 (x64) kioku runs natively as a **client**: hooks for Claude Code
(the CLI and the Claude desktop app's Code tab) and the Codex desktop app / CLI,
plus the `kioku mcp` bridge. The server stays on a Mac or Linux machine. Run
`kioku invite` on the server and paste its Windows line in PowerShell (5.1 or 7,
normal or administrator — it always installs for your own user):

```powershell
$env:KIOKU_JOIN='192.168.1.240:7391/K7Q2M9XD'; irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1 | iex
```

Paste it into **PowerShell**. From Git Bash, paste the `curl … | sh` line instead: it hands over to PowerShell by itself. Do not wrap either line in `powershell -ExecutionPolicy Bypass -c …`: Windows Defender flags that shape as `Trojan:Win32/Commando.A!ml`.

`install.ps1` downloads `kioku-<tag>-x86_64-pc-windows-msvc.tar.gz`, verifies it
against `SHA256SUMS` (`Get-FileHash`), installs `kioku.exe` to
`%LOCALAPPDATA%\Programs\kioku`, adds that directory to your user `PATH` (and to
the open window, so `kioku` works right away; `-NoPath` opts out) and runs
`kioku join`. The manual alternative, with the token from
`kioku setup --print-client-command`:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1))) -ClientOnly http://<server>:7391 <token>
```

Options: `-Version <tag>`, `-InstallDir <dir>`, `-Repo <owner/name>` (same
environment variables as `install.sh`), `-Join <url> <code>`, `-ClientOnly <url>
<token>`, `-NoSetup` and `-NoPath`. Other arguments go to `kioku setup` /
`kioku join`. `kioku setup` without `--client-only` refuses to run on Windows,
and `kioku service` is not available there.

- **Hooks:** Claude Code gets the exec form (`"command": "C:\\…\\kioku.exe",
  "args": ["hook", "stop"]`), so no shell is involved; Codex gets a `command`
  plus a PowerShell `commandWindows` (`& "C:\…\kioku.exe" hook stop --agent
  codex`); Cursor (best effort) a quoted command string. Gemini CLI and
  Antigravity are not set up on Windows. Hook input is read as UTF-8 bytes (a
  BOM is ignored), so Japanese prompts survive on a Japanese-locale Windows.
- **Updates:** `kioku update` works as elsewhere; a running `kioku.exe` cannot be
  overwritten, so it is renamed to `kioku.exe.old` and removed by the next run.
- **SmartScreen:** the Windows binary is not code-signed yet; Windows may warn
  the first time the downloaded `kioku.exe` runs.
- **Permissions:** files under `%USERPROFILE%\.kioku` inherit your profile's
  ACL (only you and administrators); the unix 0600/0700 modes do not apply.
- **WSL and Orca:** an agent running inside WSL (Codex's "Agent environment =
  WSL", Orca's WSL terminals) runs Linux processes; install the Linux kioku
  inside WSL by pasting the macOS / Linux line of `kioku invite`. Orca gives a WSL-hosted Codex its
  own isolated home, which may not see `~/.codex/hooks.json` (known limitation).
  A native Windows agent working in a `\\wsl.localhost\<distro>\…` repository
  should get the same project id through the git remote, but this is untested.

## `kioku setup`

```
kioku setup [--client-only <url> <token>] [--no-service] [--no-agents] [--agents a,b]
            [--bind <addr>] [--no-instructions] [--dry-run] [--print-client-command]
```

One idempotent, non-interactive step (safe under `curl … | sh`; run it again
any time):

1. **config** — creates `~/.kioku/config.toml` with a new token (like `kioku
   init`), or keeps the existing one (the token is never replaced).
   `--bind 0.0.0.0` for a server other machines connect to. With
   `--client-only`, the URL and token are checked against the server *before*
   anything is written, and only a `[client]` section is written.
2. **service** — installs the background service (`kioku service install`,
   below) and waits for the server to answer. If a kioku server already answers
   on the port (e.g. Docker), no service is installed. If the installed service
   answers with another version than this binary (you just replaced the
   binary), it is restarted: `restarted (v<old> -> v<new>)`. `--no-service`
   skips it.
3. **auth** — checks the token with an authenticated request.
4. **agents** — installs hooks + MCP for every agent it detects (`~/.claude`,
   `~/.codex` or `$CODEX_HOME`, `~/.cursor`, `~/.gemini`); `--agents` limits
   the set, `--no-agents` skips it, `--no-instructions` skips the instruction
   snippets.
5. **summary** — one line per step (`ok`, `--` skipped, `!!` warning, `xx`
   failed); exit 1 if a step failed.

`--dry-run` prints the plan and writes nothing. Restart running agents
afterwards so they load the new hooks and MCP server.

## `kioku doctor`

```sh
kioku doctor                  # [ OK ] / [WARN] / [FAIL] per check, with a `fix:` hint
kioku doctor --agent codex    # one agent only (even if it is not detected)
kioku doctor --json           # {"checks":[{id, status, message, fix?}]}
```

Checks the binary and `PATH`, `config.toml` (and its 0600 mode), the data dir,
`git`, the server (reachable, same version), the token, the index version, MCP,
the service, and for every detected agent: hooks present and pointing at an
existing binary, the MCP entry (the stdio bridge's binary, or for the URL form its URL and token — compared, never printed),
agent-specific switches, and the instruction snippet. Also flags recent hook
errors in `hook.log` and an enabled payload dump. Exit 1 if any check fails.

## `kioku service`

```sh
kioku service install      # write the definition, enable, start (idempotent)
kioku service status       # installed? active? pid; server health
kioku service start|stop    # start restarts a running launchd job (kickstart -k)
kioku service logs [-f] [-n 200]   # tail ~/.kioku/logs/serve.log
kioku service uninstall
```

A user-level service running `kioku serve --log-file ~/.kioku/logs/serve.log`,
no sudo: on macOS the LaunchAgent
`~/Library/LaunchAgents/dev.kioku.serve.plist` (launchd), on Linux the
`systemd --user` unit `~/.config/systemd/user/kioku.service`. On Linux it
enables lingering (`loginctl enable-linger`) so the server keeps running
after you log out; if that is not allowed it prints the command to run
yourself. Without launchd or a working `systemctl --user` (WSL without systemd,
containers) it prints how to run `kioku serve` yourself (or use Docker) and
`setup` continues with a warning. Client-only machines have no service.
The service is restarted after a crash but not after a clean exit, and a
crash loop is throttled (launchd: 10 s between starts; systemd: at most 5
starts per 60 s). `serve.log` rotates at 10 MiB (`.1`–`.3` kept).

## Agents

`kioku setup` installs every detected agent; `kioku install <agent>` installs
one (`claude-code`, `codex`, `cursor`, `gemini-cli`, `antigravity`, or `all`), and `kioku
uninstall <agent>` removes exactly what kioku added. Common to all:

- hooks run `<absolute path of kioku> hook <event> --agent <agent>`, so they
  work regardless of `PATH` (install kioku in a stable place such as
  `~/.local/bin`, not `target/`); they are fail-open (see Security notes);
- entries are merged: other hooks and servers in the same files are kept, a
  second run changes nothing, and each existing file is backed up once to
  `<file>.kioku-bak` before kioku first changes it; a file that is not valid
  JSON is left untouched and the snippet to add is printed;
- a config file that is a symlink (e.g. into a dotfiles repository) is edited
  through the link: the link stays, its target is updated (backup next to
  the target). When kioku adds the token to a file others can read, it sets
  the file to 0600 and says so;
- the MCP server is registered as the **`kioku mcp` stdio bridge**
  (`{"command": "<kioku>", "args": ["mcp"]}`): the agent starts kioku, which
  relays each tool call to the server in `~/.kioku/config.toml`. No agent file
  holds the server URL or the token, the bridge uses the same resilient
  connection as the hooks (last-good addresses, IPv6/IPv4), and changing
  server is just `kioku setup --client-only …`. `--mcp-http` registers the old
  URL + token form instead;
- `--project` writes hooks (and instructions) into the current repository
  instead; the MCP entry always stays in your user config, never in a
  repository. Project hook files contain a machine-specific
  path — do not commit them. `--project` refuses to run when the project
  directory is your home directory;
- `--dry-run` shows what would change.

### Claude Code

| what | where |
|------|-------|
| hooks (SessionStart, UserPromptSubmit, PostToolUse, PreCompact, Stop, SessionEnd) | `~/.claude/settings.json` (`--project`: `./.claude/settings.json`) |
| MCP | `mcpServers.kioku` in `~/.claude.json`: `{"type": "stdio", "command": "<kioku>", "args": ["mcp"]}` |
| instructions | none by default (the hooks inject context); `--instructions` adds a block to `~/.claude/CLAUDE.md` |

kioku does not run `claude mcp add`, so the token never appears on a command
line.

### Codex CLI

| what | where |
|------|-------|
| hooks | `~/.codex/hooks.json` (`$CODEX_HOME`; `--project`: `<repo>/.codex/hooks.json`) |
| MCP | a managed block `[mcp_servers.kioku]` (`command` + `args = ["mcp"]`) in `~/.codex/config.toml`; bytes outside the block are never changed, and tables Codex itself later adds inside it (project trust, hook trust) are moved out of it, never deleted |
| instructions | a delimited kioku block in `~/.codex/AGENTS.md` (`--project`: `<repo>/AGENTS.md`) |

**Trust step:** Codex runs a new or changed hook only after you trust it.
Open Codex once and run `/hooks`, then trust kioku's hooks (setup and doctor
remind you). kioku never writes trust state for you; the path stays the same
across `kioku update`, so this is needed again only if the binary moves. Until
then the AGENTS.md block tells Codex to use the kioku MCP tools. Hooks are on
by default in current Codex; for an older build that still needs the feature
flag, `kioku install codex --enable-hooks-feature` adds `hooks = true` under
`[features]`.

### Cursor

| what | where |
|------|-------|
| hooks | `~/.cursor/hooks.json` (`--project`: `<repo>/.cursor/hooks.json`) — used by the editor and the `agent` CLI |
| MCP | `mcpServers.kioku` in `~/.cursor/mcp.json` |
| instructions | with `--project` only: `<repo>/.cursor/rules/kioku.mdc` (Cursor has no user-level rules file) |

**Duplicate hooks:** Cursor also runs Claude Code's hooks from
`~/.claude/settings.json` ("Include Third-Party Plugins, Skills, and Other
Configs", on by default). When kioku's native Cursor hooks are installed, the
imported Claude Code invocations recognise that they run inside Cursor and do
nothing, so nothing is recorded twice; without native hooks they are handled
as Cursor events. Install both (setup does) and let `kioku doctor` confirm
(`agent.cursor.duplicate`). Because Cursor's sessionStart context is not
always delivered, kioku also adds its context on the first tool use of each
session (`postToolUse`; file edits and failed tools cannot carry it;
`[client] cursor_late_context = false` turns that off).

### Gemini CLI

Google retired Gemini CLI for personal accounts on 2026-06-18 (it still serves
Code Assist Standard/Enterprise and paid API keys); its successor is
Antigravity CLI, below. Gemini CLI is detected by `~/.gemini/tmp`, not by
`~/.gemini`, which Antigravity creates too.

| what | where |
|------|-------|
| hooks (named `kioku-*`) | `hooks` in `~/.gemini/settings.json` (`--project`: `<repo>/.gemini/settings.json`) |
| MCP | `mcpServers.kioku` in `~/.gemini/settings.json` (`--trust-mcp` adds `"trust": true`) |
| instructions | a delimited kioku block in `~/.gemini/GEMINI.md` (`--project`: `<repo>/GEMINI.md`) |

Hooks must not be disabled (`hooksConfig.enabled: false`, or a `kioku-*` name
in `hooksConfig.disabled`); doctor checks both. Project hooks show Gemini's
one-time warning before they first run. If `context.fileName` makes Gemini
read `AGENTS.md` (shared with Codex), uninstalling one of the two keeps the
kioku block while the other is still installed.

### Antigravity CLI

`agy` (detected by `~/.gemini/antigravity-cli` or `~/.local/bin/agy`; the
desktop app alone does not count). Details and sources: `docs/SPEC-M2.1.md`.

| what | where |
|------|-------|
| hooks (the named group `kioku`) | `~/.gemini/config/hooks.json` (`--project`: `<repo>/.agents/hooks.json`) |
| MCP | `mcpServers.kioku` with `serverUrl` in `~/.gemini/config/mcp_config.json` |
| instructions | a delimited kioku block in `~/.gemini/GEMINI.md` (`--project`: `<repo>/AGENTS.md`) |

agy loads only SessionStart, PreInvocation, PostInvocation and Stop, and its
payloads carry neither the prompt nor a cwd. So kioku reads each new prompt
from the conversation transcript, delivers the handoff on the first model
call of a conversation (`injectSteps`), counts later model calls of a turn as
tool rounds for the Stop nudge, and takes the project from `workspacePaths`.
`agy -p` without `--add-dir` sends no workspace, so such runs are not recorded.
Other tools' groups in `hooks.json` are kept; check what agy loaded with
`agy -p "/hooks" --output-format json`.

## Try it

1. Open any of these agents in a git repository and give it a task. `kioku
   project id` prints the project id kioku uses for that directory.
2. When it finishes a turn after using a few tools (since its last handoff),
   the Stop nudge asks it to call `kioku_handoff_write`.
3. Start a new session in the same repository (or `/clear`) — in the same or
   another agent, on this or another machine: the handoff is injected at
   SessionStart.
4. Search from the terminal:

```sh
kioku search 引き継ぎ
kioku search --project <id> --limit 5 設計 判断
kioku status
```

## Home server

Run one server for all your machines. On a fresh server,
`curl -fsSL …/install.sh | sh -s -- --bind 0.0.0.0` writes `[server] bind =
"0.0.0.0"` into the new `config.toml` (an existing config is kept: edit `bind`
there, then `kioku service stop && kioku service start`). Then run
`kioku invite` for every other machine (see Install).

**Which URL to use.** `kioku invite` (like `--print-client-command`) prints the
server's LAN IP. The pasted line hands the new machine exactly the address it
used to download the script, so whichever you choose is what it keeps.
- The IP also works over a VPN that routes the LAN (WireGuard), but breaks if the server's address changes.
- `<host>.local` survives address changes, but it is mDNS, so it only resolves on the LAN itself.
- With `bind = "0.0.0.0"` the server listens on IPv4 and IPv6, so a name that resolves to IPv6 still reaches it.
- Hooks and the CLI remember the addresses that worked for a named server in `~/.kioku/state/server-addrs.json` and try them first, so they keep working over a VPN where the name no longer resolves.
- Agents' MCP goes through the `kioku mcp` bridge, so it gets the same last-good addresses as the hooks. (Agents installed with `--mcp-http` connect to their configured URL directly: use an IP or a DNS name there.)

**On a macOS server, check your firewall.** With Little Snitch, LuLu or similar installed, LAN
connections to a new kioku binary wait on an allow prompt that shows only on the server's own
screen: clients connect but never get an answer. `kioku doctor` checks this as `server.lan`
(health over the machine's LAN address). After allowing kioku, run
`kioku service stop && kioku service start`.

Hooks never send requests to a server on this machine or a private network
through an `HTTP(S)_PROXY` from the environment (loopback, 10/8, 172.16/12,
192.168/16, fc00::/7, fe80::/10, and `*.local` / `*.lan` / `*.internal`
names). For any other host (e.g. a VPN name like `kioku.tailnet.ts.net`) that
must not go through your proxy, add it to `NO_PROXY`.

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
the token — clients need it for `install.sh … --client-only <url> <token>` (or
`kioku setup --client-only`). `docker exec kioku kioku invite` also works; it
prints the container's address, so replace it with the Docker host's in the
pasted line. On the Docker host itself, `kioku setup` sees the
running server and does not install a service. A bind-mounted host
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
| `kioku_handoff_write` | `project`, `session?` (from the SessionStart block), `summary`, `next_steps`, `open_questions`, `decisions` | records the handoff the next session of the project receives |
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
  index/schema-version          # index format; an older one → run `kioku reindex`
  logs/hook.log                 # client-side hook failures
```

Pages are Markdown with YAML frontmatter; you can read and edit them with any
editor (run `kioku reindex` afterwards so search sees the change). kioku commits but never pushes: backing up (for example, pushing
`wiki/` to a private remote, and copying `db/`, where handoffs live) is up to
you.

Page file names are the ASCII slug of the title; when slugging drops anything
(non-ASCII, punctuation, repeated separators — `C++ tips` vs `C tips`) a
6-hex hash of the title is appended so different titles never share a file.
Search normalizes text with NFKC, so full-width `Ｆｌｕｔｔｅｒ` and half-width
`ｱﾌﾟﾘ` match `flutter` / `アプリ`. **After upgrading kioku, run `kioku reindex`**
if the server log warns that the index was built by an older version.

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
  and the invite routes below requires `Authorization: Bearer <token>`,
  including `/mcp`, whose Host-header allowlist is disabled so the token is the
  guard. The default bind is `127.0.0.1`.
- **Invites**: `kioku invite` (bearer-authenticated `POST /api/v1/invites`)
  creates an 8-character code, held in the server's memory only, valid 10
  minutes and once by default. `GET /i/<code>` and `GET /i/<code>.ps1` serve the
  installer (no token in it) and `POST /api/v1/join` trades the code for the
  token; these three need no token. More than 10 failed code lookups a minute
  from one address (30 from all) get HTTP 429 for 60 seconds. Anyone who sees
  an unused invite line can join, so treat it like the token for its 10
  minutes; over plain HTTP the token crosses the network once, as it does with
  every hook request.
- **Rotating the token**: `kioku rotate-token` on the server machine writes a
  new token, restarts the service (the old token is rejected from then on).
  Then run `kioku invite --uses <n>` and paste its line on every other machine
  (it also prints the manual `kioku setup --client-only <url> <new token>`).
  Agents hold no token since v0.4 (`kioku mcp`), so that is all.
- Agent files that hold the token (`~/.claude.json`, `~/.codex/config.toml`,
  `~/.cursor/mcp.json`, `~/.gemini/settings.json`) are created 0600; an
  existing one that others could read is set to 0600 when kioku adds the token
  (reported in one line; the token itself is never printed).
- `config.toml` holds the token in plain text; kioku writes it with mode 0600
  and creates the data dir, `raw/` and `logs/` as 0700 (unix).
- **Sanitizer** — hook payloads are redacted on the client before they are
  sent (and again by the server):
  - AWS access key ids (`AKIA…`), `sk-…` keys, Stripe `sk_live_…` /
    `sk_test_…` keys, GitHub `ghp_` / `gho_` / `ghu_` / `ghs_` / `ghr_` and
    `github_pat_…` tokens, Slack `xoxb-` / `xoxa-` / `xoxp-` tokens, Google
    `AIza…` keys, JWT-shaped strings (`eyJ….….…`), PEM private key blocks
    (to the end of the text when the `END` line is missing);
  - the password in URLs (`postgres://user:[REDACTED]@host`);
  - the whole value after any key *containing* `secret`, `token`,
    `password`/`passwd`, `api_key`, `access_key`, `private_key`, `credential`
    or `authorization` followed by `:` or `=` (`AWS_SECRET_ACCESS_KEY=…`,
    `"access_token": "…"`, `password = "several words"`; quoted values are
    one unit, `Bearer …` included), the value after `--password`, `--token`,
    `--api-key`, and JSON values under keys containing those words (except
    counts such as `max_tokens`);
  - `tool_input` is truncated to 4 000 chars and `tool_response` to 2 000.
- **Not redacted**: anything that does not match those shapes — e.g. a
  password passed as `-p secret`, bare high-entropy strings, personal data.
  Prompts, commands, file paths and
  the (truncated) output of `Read`/`Bash`/edit tools reach the server and end
  up in `raw/`, SQLite and, in digested form, the git history of `wiki/`.
  Transcripts are not uploaded.
- **Hooks are fail-open**: on any network or server error a hook logs one line
  to `logs/hook.log` (capped at 1 MiB, one rotated `hook.log.1`), prints nothing and exits 0 within `timeout_ms`, so a
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

- **M1**: server, Markdown/git store, Japanese search, MCP tools, Claude Code
  hooks and handoffs, rule-based summaries.
- **M2** (this release): Codex CLI, Cursor and Gemini CLI; `install.sh`,
  `kioku setup` / `doctor` / `service` / `update`.
- **Later**: web UI.
- **M3**: embeddings + bi-temporal facts.
- **M4**: ingest adapters.
- **M5**: eval harness.

## License

MIT OR Apache-2.0, at your option ([LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE)).
