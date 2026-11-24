# 0029 — No keyboard hook by default on the Windows viewer

**Status:** accepted · **Date:** 2026-11-24

## Context
The Windows viewer used a low-level keyboard hook (`WH_KEYBOARD_LL`) for
the whole session, re-installed every 3 s, so Windows' own shortcuts (Win,
Alt+Tab, Ctrl+Esc) went to the host. Microsoft Defender quarantined
Glass.exe twice as `Behavior:Win32/CredentialAccess.A!ml` (m4-test1, and
m5-test13 21 s into viewing the Mac): an unsigned program hooking the
keyboard and sending keys over the network looks like a keylogger. Glass
has no trusted signature to vouch for it (ADR 0008), and people installing
it won't have an exclusion.

## Decision
- **No hook by default.** The viewer window's own key messages go to the
  host: everything except the few shortcuts Windows keeps for itself.
- **A Keys menu in the viewer's bar** sends those on a click: for a PC
  host Start (Win), Alt+Tab, Win+D, Win+E, Win+Shift+S, Ctrl+Shift+Esc;
  for a Mac host Spotlight, ⌘Tab, Mission Control, the screenshot and
  Force Quit shortcuts.
- **Settings → Windows shortcuts → Sent directly** turns the hook back on
  (`--hook-shortcuts` on the command line), and then only while Glass is
  the app in front (WM_ACTIVATEAPP), with no periodic re-install. The
  setting says Defender may flag it.
- Ctrl+Alt+Del needs SendSAS on the host (an open item); it isn't in the
  menu yet.

## Consequences
- Holding Win or Alt+Tab directly no longer reaches the host by default.
  With a Mac host that matters little (Ctrl already acts as ⌘).
- Whether Defender stays quiet can only be seen over time; detections were
  occasional.
