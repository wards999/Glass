# 0015 — The Windows service (unattended access), moved up as M3f

**Status:** accepted · **Date:** 2026-11-21

## Context
The PC → PC test (M3d) failed in a way that comes up whenever a host is far
away: QUIETPC was being used through Windows Remote Desktop. No capture API
can see a Remote Desktop session, and the PC's own screen sat at the lock
screen, which a normal app can neither show nor control. The SYSTEM service
was parked for the very end (M8); the user asked to build it now. The worry:
will a service be hard to keep updating while Glass changes quickly?

## Decision
Build it as M3f, straight after M3d, and keep the service itself tiny:

- **`glass-cli service run`** is the service (LocalSystem, automatic start).
  Its only job: keep one agent running in the PC's console session (the one on
  its physical screen), starting a new one when that session changes or the
  agent stops (with back-off). It almost never needs to change.
- **`glass-cli agent`** does the sharing, as SYSTEM in that session: DXGI
  Desktop Duplication (`dda.rs`), which, unlike Windows.Graphics.Capture, can
  capture the Winlogon desktop (lock screen, sign-in, UAC). The capture and
  input threads follow the input desktop (`desktop.rs`) and capture restarts
  on ACCESS_LOST. The blocker notices are off (nothing blocks SYSTEM).
- **Identity:** the agent uses `C:\ProgramData\Glass\host`, which only
  SYSTEM and administrators can read, with machine-scope DPAPI. The installer
  (elevated, as the same user) re-encrypts this user's key for the machine
  and copies the paired devices, so pairings carry over.
- **The Glass window** reads `C:\ProgramData\Glass\status.txt` (readable by
  all, written only by SYSTEM). It shows "Always on · …", who's connected and
  the pairing code. Its own Share switch steps aside: the service has the
  port.
- **Install / update / remove:** `glass-cli service install | uninstall
  [--purge]` as administrator, also from Settings > Unattended access. This
  copies the build to `C:\Program Files\Glass`, restricts the identity folder
  (icacls with well-known SIDs), registers the service with restart-on-failure,
  adds a Windows Firewall rule for private networks, and starts it. Installing
  again from a newer build updates it.
- **Remote Desktop:** the window says when it's running in a Remote Desktop
  session. Without the service it offers "Move it to the PC's screen" (`tscon
  … /dest:console`, elevated).

## Consequences
- Updates are cheap: almost every build only changes the files the service
  launches. The service code is a few hundred lines that rarely change.
- Installing changes system state (a service, a firewall rule, Program Files),
  always explicitly and as administrator, and is fully reversible.
- Desktop Duplication needs the monitor to be on the encoding GPU. Hybrid
  laptops may need LATER work.
- LATER(M3f): Ctrl+Alt+Del (SendSAS) for PCs set to require it; updates
  through the service without an admin prompt (verified against the
  release's published hashes); a Mac equivalent (LaunchDaemon) later.
