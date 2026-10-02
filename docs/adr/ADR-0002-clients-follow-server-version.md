# ADR-0002: The server follows releases; clients follow the server

Status: accepted (SPEC-M2.5, v0.7.0, 2026-09-30; recorded 2026-10-02 in SPEC-M3.2 §4)

## Context

One person runs one kioku server and several clients (laptop, desktop, Windows PC). Each
machine updating on its own schedule leaves the fleet on mixed versions, and a client newer
than its server may send what the server does not understand. Asking the user to update
every machine by hand defeats "one command, then forget it".

## Decision

- The **server** checks GitHub releases itself (only when it runs under the service
  manager): first 3 minutes after start, then every `[update] interval_hours` (default 1
  since v0.9.1), verifies the release (checksum → signature → `--version`, SPEC-M2.7 §2),
  swaps its binary and exits 75 so launchd / systemd restart it (SPEC-M2.5 §3.1). A new
  binary that fails to start three times is rolled back (SPEC-M2.7 §7).
- **Clients never ask GitHub on their own.** At SessionStart a client compares its version
  with the server's `server_version`; when the server is newer it updates *to that
  version* in a detached background process (SPEC-M2.5 §3.3). A client never downgrades.
- `[update] auto = false` turns both into a notice only; `kioku update` stays the manual
  path.

## Consequences

- The fleet converges on the server's version within one session start per machine.
- Protocol changes only need to be compatible in one direction: a newer server with an
  older client for at most one session (new fields are additive, `serde(default)`).
- A server without internet keeps its clients where it is.
- A bad release reaches every machine; the defences are verification before execution,
  rollback on failed starts, and `kioku update --rollback`.
