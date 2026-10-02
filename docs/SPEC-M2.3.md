# kioku — SPEC-M2.3: one-command join (`kioku invite`)

Status: implemented (branch `m2.3-invite`), 2026-09-28; see §8 for what changed. Amends SPEC-M2 (and M2.2). Read CLAUDE.md, SPEC-M2.md
§11, §13, §19–§21 and SPEC-M2.2 first.

> **Amended by SPEC-M2.7 §4 (v0.9):** `GET /i/<code>` and `GET /i/<code>.ps1` are gone
> (a former route answers 404 with an `echo …; exit 1` body). The macOS / Linux line is
> now `KIOKU_JOIN='<host:port>/<CODE>' sh -c "$(curl -fsSL
> https://raw.githubusercontent.com/misorafa/kioku/main/install.sh)"` — the script from
> GitHub over https, only the code over the LAN, like the Windows line of §9.
> `kioku invite --host <addr>` overrides the printed address; otherwise the machine's
> other IPv4 addresses are listed (LAN first). `POST /api/v1/join` is unchanged.

## 1. Why

Adding a machine must be **one pasted line that leaves kioku fully usable**, for
people who are not comfortable with terminals. The first real Windows 11 install
(2026-09-28) broke that promise in several ways:

- the PowerShell line `& ([scriptblock]::Create((irm …))) -ClientOnly <url>
  <64-hex token>` was long and fragile, and copying it dropped the leading `&`;
- the auth token had to be copied by hand, it ended up in a screenshot, and it had
  to be rotated;
- `kioku` was not on PATH after the install, because the installer only printed a
  hint;
- admin vs normal PowerShell needed explaining;
- after `rotate-token`, updating a Windows client needed ssh plus shell variables.

Principle for everything below: the person on the new machine pastes one short line
and does nothing else. The **server owner** runs one command to get that line. No
token is ever shown or copied.

## 2. User flow

On the server machine:

```
$ kioku invite
Paste ONE of these on the machine to add (valid 10 minutes, once):

  Windows (PowerShell):  $env:KIOKU_JOIN='192.168.1.240:7391/K7Q2M9XD'; irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1 | iex   (since v0.6.1, §9)
  macOS / Linux:         curl -fsSL http://192.168.1.240:7391/i/K7Q2M9XD | sh

(On this LAN you can also use http://mini-M2.local:7391/…; over a VPN use the IP.)
```

On the new machine the user pastes the line. That one command:

1. downloads and verifies kioku from GitHub Releases (as today);
2. installs it and puts it on PATH;
3. exchanges the invite code for the server's token;
4. writes a client-only config.toml;
5. registers every detected agent (hooks + `kioku mcp`);
6. ends with a short bilingual message: "kioku の準備ができました。Claude / Codex /
   Cursor などを再起動してください。 / kioku is ready — restart Claude, Codex,
   Cursor, …".

A pasted line that is broken, expired or already used fails with one clear
sentence that says to run `kioku invite` again on the server.

After `kioku rotate-token`, the owner runs `kioku invite` again and pastes the
new line on each machine. `join` replaces the old client config, so there is no
ssh and no token handling.

## 3. Server

### 3.1 Invites (in memory)

- An invite is `{code, created_at, expires_at, uses_left}`. It is held in memory
  in the server's shared state (`Arc<parking_lot::Mutex<…>>`). A server restart
  drops all invites, which is fine for a 10-minute object.
- **Code:** 8 characters from Crockford base32 without the ambiguous letters
  (`0-9A-Z` minus `I L O U`), shown upper-case and matched case-insensitively.
  It comes from `util::generate_token`'s random source.
- Defaults are a 10 minute TTL and 1 use. `--ttl <minutes>` is capped at 60 and
  `--uses <n>` at 20, for adding several machines at once.
- Expired invites are purged on every access.

### 3.2 Routes

> Amended by SPEC-M2.7 §4: `GET /i/<code>` and `GET /i/<code>.ps1` are gone; only `POST /api/v1/join` remains.

| route | auth | does |
|-------|------|------|
| `POST /api/v1/invites` `{ttl_minutes?, uses?}` | **bearer token** | creates an invite and returns `{code, expires_at, uses}` |
| `GET /i/<code>` | none | a valid, unexpired invite returns the **sh** installer (§4) with `text/plain`; otherwise 404 with a one-line sh `echo … >&2; exit 1` explaining what to do |
| `GET /i/<code>.ps1` | none | the same, for **PowerShell** |
| `POST /api/v1/join` `{code}` | none | consumes one use and returns `{token, server_url}`; invalid, expired or used → 404 `{error}` |

Rules:

- The installer script is the repo's `install.sh` / `install.ps1`, embedded in the
  server binary (`include_str!`). It gets two variables prepended:
  `KIOKU_JOIN_URL` and `KIOKU_JOIN_CODE` (sh), or `$KiokuJoinUrl` and
  `$KiokuJoinCode` (ps1).
- `KIOKU_JOIN_URL` is `http://<Host header>` of the request that fetched the
  script. That is the address the new machine used to reach the server, so it
  is right for LAN IP, `.local` and VPN alike. A missing or malformed Host header
  gets a 400.
- Fetching the script does **not** consume the invite; only `/api/v1/join` does,
  so a retried download still works.
- **Rate limit** on the unauthenticated routes: at most 10 failed code lookups
  per minute per peer IP and 30 overall. Past that the server answers 429 for 60
  seconds. With 32^8 codes, a 10-minute TTL and one use, guessing is hopeless.
- `server_url` in the join response is the same Host-derived URL. The token is
  the server's current `[server] auth_token`.
- These routes are **not** under the bearer middleware. Every other route stays
  protected. `serve` still refuses a non-loopback bind without a token.

## 4. Installers

### 4.1 Join mode

- install.sh with `KIOKU_JOIN_URL`/`KIOKU_JOIN_CODE` set (or `--join <url>
  <code>`) installs as today and then runs `kioku join <url> <code>`. The
  pasted pipe `curl … | sh` needs no arguments.
- install.ps1 with `$KiokuJoinUrl`/`$KiokuJoinCode` set (or `-Join <url>
  <code>`) does the same with `kioku.exe join`. The pasted `irm … | iex` needs no
  arguments and no `&`/scriptblock.

### 4.2 PATH on by default

- **Windows:** install.ps1 adds the install dir to the **user** PATH by default
  (`[Environment]::SetEnvironmentVariable(…, 'User')`) and also to the current
  session's `$env:Path`. `-NoPath` opts out. It says what it did.
- **macOS / Linux:** install.sh adds `export PATH="$HOME/.local/bin:$PATH"` to the
  user's shell rc when the dir is not on PATH: `~/.zshrc` for zsh, `~/.bashrc` for
  bash, else `~/.profile`. It writes one marked line and is idempotent.
  `--no-modify-path` opts out. This reverses install.sh's old "never edits shell
  files" rule, which the one-command principle overrides. README and SPEC-M2
  §13.2 are updated.

### 4.3 Robustness

- install.ps1 works in both an elevated and a normal PowerShell, and in both
  Windows PowerShell 5.1 and pwsh 7. It installs for the invoking user either
  way; elevation is only mentioned, never an error.
- Messages at the end are bilingual (ja + en), short, and list the agents that
  were set up.

## 5. CLI

- **`kioku invite [--ttl <minutes>] [--uses <n>]`**
  - Runs on the server machine (config has `[server]`). It calls `POST
    /api/v1/invites` on its own server with its own token.
  - It prints the two lines of §2, using the URL rule of `setup::client_url`
    (LAN IP) and the `.local` alternative.
  - On a client machine it refuses: "run kioku invite on the server machine".
- **`kioku join <url> <code>`**
  - Calls `POST <url>/api/v1/join`, then does exactly what `setup
    --client-only <url> <token>` does. It shares the implementation and never
    prints the token.
  - On a former server machine the §11 "retire [server]" rule applies as usual.
  - A failure prints one sentence: code expired or used → "ask for a new
    `kioku invite`"; unreachable → the URL and the firewall hint.
- **`--print-client-command`** keeps working, but its output now recommends
  `kioku invite`.

## 6. Tests (same standards as before; Japanese where search is involved)

- **Server:** invite creation needs the token; the code format and the
  case-insensitive match; script served with the right variables and the
  Host-derived URL; 404 for a bad or expired code; join consumes uses
  (`uses=2` works twice, the third call is 404); expiry, with an injectable
  clock; the rate limit gives 429; protected routes still need the token.
- **CLI:** e2e `kioku invite` → `kioku join` against a test server writes the
  client config with the right token and never prints it (assert the token is
  absent from stdout/stderr); refusals.
- **install.sh** (`scripts/test-install.sh`): join mode via env vars, and PATH
  added to the right rc exactly once; `--no-modify-path`.
- **install.ps1** (`scripts/test-install.ps1`, windows-latest): join mode via
  variables, user PATH set, `-NoPath`, and it runs under both 5.1 and 7.

## 7. Deliverables (for the implementing session)

Branch `m2.3-invite`, a draft PR against `main`, CI green on every job including
Windows, and README / README.ja rewritten so the **first** install instruction
for a second machine is "run `kioku invite` on the server, paste the line". The
old `--client-only` path stays documented as the manual alternative. Update this
spec with anything that turned out different. Do not merge, tag or change
secrets.

## 8. Implementation notes (what turned out different)

Recorded by the implementing session; each choice follows §1's principle.

1. **`curl -sSL`, not `curl -fsSL`, in the printed line.** With `-f` curl
   swallows the body of a 404/429, so `sh` would get an empty script and the
   user would see only `curl: (22) …`. Without `-f` the failure body (the
   one-line `echo …>&2; exit 1` of §3.2) runs and prints the one clear sentence.
2. **PowerShell failure bodies are plain text, not a script.** `irm` never
   pipes an error response into `iex`; both Windows PowerShell 5.1 and pwsh 7
   show the body of an error response as the error message. So `/i/<code>.ps1`
   answers 404/429/400 with the ja + en sentences.
3. **The `.ps1` script is wrapped in `& { … }`.** `irm … | iex` runs in the
   caller's scope, so a bare install.ps1 would leave `$ErrorActionPreference =
   'Stop'`, its functions and `$KiokuJoinUrl` in the user's window (a later
   plain install.ps1 run there would re-join with a used code). The server emits
   `& {`, the two variables, install.ps1, `}`. The user PATH change and
   `$env:Path` of the window are process-wide and survive the block.
4. **Host rule.** `GET /i/<code>` accepts `/i/<code>`, `/i/<code>.sh` and
   `/i/<code>.ps1` (axum routes one segment; the suffix is parsed in the handler).
   The Host header must be `name[:port]` or `[ipv6][:port]` from `[A-Za-z0-9.-]`
   (checked before it is pasted into single-quoted strings); anything else is a
   400 with the same kind of body as a 404. Without a Host header the URI
   authority is used (HTTP/2).
5. **Rate limit details.** Failed lookups on all three public invite routes
   count. The 10th failure from one peer within 60 s blocks that peer, the 30th
   overall blocks everyone, for 60 s; a blocked peer gets 429 even for a valid
   code. The peer is the socket address (`into_make_service_with_connect_info`;
   IPv4-mapped IPv6 as IPv4). `POST /api/v1/invites` answers 409 when the server
   has no token (loopback-only, auth disabled): there is nothing to hand out.
6. **`kioku join` uses the `<url>` it was given** for `[client] server_url`
   (it is the same Host-derived URL as `server_url` in the join answer, and it
   is the address this machine has just reached). A missing `http://` is added.
   Flags: `--agents`, `--no-agents`, `--no-instructions`, `--mcp-http` (passed to
   the shared setup code); install.sh / install.ps1 pass extra arguments to it.
   The output is the setup summary plus the bilingual final message naming the
   agents that were set up; as a last guard the token is replaced by `<token>`
   in anything printed.
7. **`kioku invite` in Docker.** With no config.toml but `KIOKU_AUTH_TOKEN`
   set, the machine counts as a server (the printed address is the
   container's; README says to replace it with the host's). It talks to its own
   server at `127.0.0.1:<port>` (`[::1]` for `bind = "::"`, the bind address
   when it binds one address). Errors name the fix: server not running → `kioku
   service start`; 404 on `/invites` (server older than the CLI, e.g. right
   after an update without restart) → restart the service.
8. **PATH on macOS / fish.** bash on macOS writes `~/.bash_profile` (Terminal
   starts login shells, which do not read `~/.bashrc`); fish gets
   `~/.config/fish/conf.d/kioku.fish` (`contains … ; or set -gx PATH …`), since
   fish reads no `~/.profile`. The rc line is
   `export PATH="$HOME/.local/bin:$PATH" # added by the kioku installer`;
   idempotence is an exact-line match; a last line without a newline gets one
   first. A directory containing `"`, `\`, `` ` `` or `$` is never written,
   only hinted.
9. **Windows PATH** is appended to the user PATH with
   `[Environment]::SetEnvironmentVariable('Path', …, 'User')` as specified
   (this rewrites the value as REG_SZ; `%VAR%` entries are stored expanded,
   which keeps them working). `-AddToPath` is still accepted and does nothing
   (it is the default). install.ps1 stays ASCII (5.1 reads a BOM-less `-File`
   script as ANSI), so its Japanese text is built from `\u` escapes. An
   elevated PowerShell gets one line saying elevation is not needed.
10. **Other pointers to `kioku invite`:** `--print-client-command` prints
    "Easiest: run `kioku invite` here …" before the manual line;
    `rotate-token` now recommends `kioku invite --uses <n>` and still prints the
    manual line; `kioku init`'s next steps, doctor's auth fix, the Windows
    `setup` refusal and install.sh's Git-Bash message mention it.
11. **Not verified on a real network:** the full `irm … | iex` / `curl … | sh`
    flow was run end-to-end on macOS against a local server and a fixture
    release (installer → `kioku join` → client config); the Windows flow is
    covered by CI with a stub binary (install.ps1) and by the Rust e2e test with
    the real binary (`invite_join.rs`), not yet by a paste on a real Windows
    11 machine. The first release containing `kioku join` must exist before the
    invite line works for users, because the installer downloads `latest`.

## 9. Windows line changed after a real Defender block (2026-09-29, v0.6.1)

On the user's Windows 11, the pasted `irm http://192.168.1.240:7391/i/<code>.ps1 | iex`
was first put into Git Bash, where `irm` does not exist. The fallback I suggested,
`powershell -ExecutionPolicy Bypass -c "irm http://<LAN IP>/i/<code>.ps1 | iex"`, was
then **blocked by Windows Defender as `Trojan:Win32/Commando.A!ml`**. Defender matched
the command-line shape (`CmdLine:`), not kioku's content. A download cradle from a
bare IP over http, together with `-ExecutionPolicy Bypass`, is a malware pattern.

Changes:

1. **The Windows line** printed by `kioku invite` is now
   `$env:KIOKU_JOIN='<host:port>/<code>'; irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1 | iex`.
   - The script comes from GitHub over https: the same path that installed v0.5.0
     cleanly on that machine.
   - Only the invite (server address plus code, no token) comes from the LAN.
   - The line has no `-ExecutionPolicy Bypass` and never starts a second
     `powershell.exe`.
2. **install.ps1**
   - It reads `KIOKU_JOIN` (`[http(s)://]host:port/code`) when no
     `$KiokuJoinUrl` is set, then removes the variable.
   - The whole file is wrapped in `& { … } @args`, so `irm … | iex` leaves no
     functions, variables or `$ErrorActionPreference` in the user's window. The
     server-served `/i/<code>.ps1` wrapping still exists.
3. **install.sh on Windows** (Git Bash / MSYS / Cygwin)
   - It no longer stops with advice. It downloads install.ps1
     (`KIOKU_PS1_URL` overrides this for tests) and runs it as
     `powershell.exe -NoProfile -Command "& ([scriptblock]::Create([IO.File]::ReadAllText('<file>')))"`,
     with `KIOKU_JOIN` set from the join variables.
   - That command line has no Bypass and no `irm | iex`, and a script block read
     from a string is not subject to the execution policy.
   - So the macOS/Linux `curl … | sh` line also works when pasted into Git
     Bash, and it is labelled "macOS / Linux / Git Bash".
4. **install.sh** also accepts `KIOKU_JOIN` for symmetry.

The `/i/<code>.ps1` route stays, for older printed lines. Code signing
(Authenticode) and a winget package remain the longer-term way to earn Windows
reputation. They are not in this change.
