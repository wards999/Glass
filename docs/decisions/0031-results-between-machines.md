# 0031 — Session results travel between paired machines

**Status:** accepted · **Date:** 2026-11-24 · protocol v16

## Context
Each test round needed the user to copy results from the Mac and QUIETPC
and paste them, since Claude works on QUIETPC22 only and can't read the
other machines' files. SSH to them would work, but opens both machines to
remote logins: far more access than testing needs.

## Decision
- A **results-only connection**: `Hello` with `CAP_RESULTS_ONLY`. After
  the usual checks (version, pairing as a viewer) the host sends its newest
  session logs (`Results` frames, kind 24) and `Bye`, and closes. No
  session, no screen change, no approval prompt.
- What it sends: the newest 8 `.txt` logs in the folders the app names
  (`core::results::add_dir`), up to the last 64 KB of each: Windows'
  `%LOCALAPPDATA%\Glass\logs` (the service's too, and as SYSTEM the users'),
  the service's `agent-results.txt`, the Mac's `~/Library/Logs/Glass`.
  Glass's own results only.
- `glass-cli results NAME|ADDRESS[:PORT]` fetches them, prints them and
  saves `%LOCALAPPDATA%\Glass\fetched\<name>.txt`.

## Consequences
- After a test, "done" is enough: QUIETPC answers always (its service is
  always sharing); the Mac while Share this Mac is on.
- A paired viewer can read a host's recent session statistics (no screen
  content, no keys typed: counts and timings).
