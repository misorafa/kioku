# Security policy

## Reporting a vulnerability

Please report security problems **privately**: on GitHub, open
[misorafa/kioku → Security → Report a vulnerability](https://github.com/misorafa/kioku/security/advisories/new)
(private vulnerability reporting / a draft security advisory). Do not open a public issue
for anything that could be exploited. Japanese or English is fine. Include the kioku
version (`kioku --version`), the OS, what you did and what happened; `kioku doctor`
output helps — **remove every line that contains a token** first.

You will get an answer within a week. Fixes are released as a new version; servers update
themselves within the hour and clients follow at their next session start
([ADR-0002](docs/adr/ADR-0002-clients-follow-server-version.md)), so a release is the fix.

Supported versions: the latest release only.

## Threat model

kioku is built for **one person**: one server, that person's machines, one bearer token.

- **Single user, one server per person.** Everyone holding the token reads and writes all of
  the memory; there are no per-user permissions. Never share a server or its token between
  people.
- **Network: LAN or a private overlay.** The server speaks **plain HTTP by design** (no
  TLS, no certificates to manage). Run it on a home LAN, or reach it over WireGuard /
  Tailscale. For anything beyond that — the internet, a shared or untrusted network — put
  it behind a reverse proxy that terminates TLS, or do not expose it. On plain HTTP the
  token crosses the network with every request.
- **Authentication.** `kioku serve` refuses to start without a token. Every route except
  `GET /api/v1/health` (answers only `ok`) and `POST /api/v1/join` (trades a one-time
  invite code for the token) requires `Authorization: Bearer <token>`, including `/mcp`
  and `/api/v1/metrics`.
- **Tokens.** `~/.kioku/config.toml` holds the token and is written 0600 (data dir 0700);
  agent config files never hold it since v0.4 (the `kioku mcp` bridge reads it from
  `config.toml`). kioku never puts a token on a command line or prints it (SPEC-M2.7 §10);
  `kioku rotate-token` replaces it and prints an invite line instead. Logs never contain it
  (tested over `serve.log`, `hook.log` and `update.log`).
- **Invite codes** are 8 characters, held in the server's memory only, valid 10 minutes and
  once by default (at most 60 minutes / 20 uses); failed lookups are rate limited. Treat an
  unused invite line like the token for its lifetime.
- **Memory is untrusted data** (SPEC-M2.7 §3). Pages, handoffs and session summaries are as
  trustworthy as what the writing agent read; a prompt injection picked up from a web page
  can be stored and shown to later sessions. kioku labels stored memory as data, not
  instructions, escapes its `<kioku>` block, and redacts secrets (API keys, tokens,
  passwords, private keys) in hook payloads, pages and handoffs — but redaction is
  pattern-based and not complete. Review a stored procedure before letting an agent run it.
- **Hooks fail open.** A hook never blocks an agent: on any error it logs one line and exits
  0 (the deliberate Stop nudge exits 2).
- **Updates run only verified binaries**: SHA-256 against the release, then (macOS) kioku's
  Developer ID signature, and only then is the binary executed; a server that fails to start
  after an update rolls back. Download mirrors must be enabled explicitly
  (`[update] allow_mirror`) and use https.

## Out of scope

- Attacks by someone who already holds the token or has a shell on the server or a client
  (they can read the memory by design).
- Plain-HTTP traffic on a network you do not control (use WireGuard / Tailscale / TLS).
- Secrets that do not match any redaction pattern (e.g. a password passed as `-p secret`).
- Denial of service against a server exposed to the internet.
- Third-party agents' own handling of the context kioku injects.
