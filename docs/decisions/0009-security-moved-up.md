# 0009 — Security moved up to right after M3

**Status:** accepted · **Date:** 2026-11-20

## Context
Since M3 a Glass host injects mouse and keyboard from the viewer. Until M6,
anyone on the same network who can reach the host's port can connect and
control the machine, with no pairing and no encryption. The user wants that
fixed now, not after gaming.

## Decision
M6 (pairing, pinned mutual TLS, encrypted UDP, per-direction permissions,
optional per-connection confirmation, mDNS discovery) comes straight after
M3 and before M3b. It's built in `core` for both directions, so the Mac host
(M3b) is secure from its first build.

## Consequences
- The PC-views-Mac work (M3b, M3c) moves one milestone later.
- Protocol v4: control channel over TLS 1.3, UDP payloads AEAD-encrypted.
  Older test builds can't connect to newer ones (already true per test build).
- Until M6 lands: only run host.cmd on a trusted network, and only while
  testing.
