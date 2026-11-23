# 0017 — The service keeps the connection; helpers do the work

**Status:** accepted · **Date:** 2026-11-23

## Context
With the Windows service (M3f, ADR 0015), the agent did everything: the
network session, capture, encoding, input and the clipboard. It ran as SYSTEM
in whichever Windows session was on the PC's screen. Signing in at the lock
screen, or Remote Desktop taking over or giving back, switches that session,
so the agent restarted and the viewer's connection dropped. It reconnected
about 5 s later. Other remote-desktop tools don't drop.

Running the clipboard as SYSTEM also caused every clipboard failure seen with
the service (m5c-test3 to test6):
- Explorer refused calls into a SYSTEM process.
- Files copied in Explorer couldn't be read.
- A SYSTEM process can't see the user's cloud-only files or mapped drives.

## Decision
Split the agent's work, the way TeamViewer and AnyDesk do:

| Part | Runs as | Lives |
|---|---|---|
| **Broker**: listener, pairing, TLS and UDP session, mDNS, status for the window | the service (SYSTEM, session 0) | as long as the service; never restarts for a session switch |
| **Screen helper** (`glass-cli agent`): capture (DDA, following the lock screen and UAC), encoding, input injection, pointer shape, resolution matching | SYSTEM, in the session on screen | restarted by the service when that session changes |
| **Clipboard helper** (`glass-cli clip-agent`): the clipboard, text, pictures and files | the signed-in user, in the session on screen | while someone is signed in there |

- The broker and helpers talk over local **named pipes**, two one-way pipes
  per helper. A synchronous pipe handle can't read and write at the same
  time from two threads.
- The messages and their framing live in **core** (`media_link.rs`), so a
  Mac version (a LaunchDaemon plus a per-user agent) can reuse them later.
  Only the pipe plumbing is Windows code.
- Screen helper ↔ broker: the broker sends the viewer's Hello (Start),
  keyframe and mode requests, input actions and the applied input sequence
  number. It also sends Stop when the viewer leaves. The helper sends its
  display size, encoded frames (broker-numbered, so ids stay continuous
  across helpers), stream changes, the pointer, notices and its part of the
  results.
- When a new screen helper connects mid-session (a session switch), the
  broker sends it Start again. Its first frame is a keyframe, and a
  StreamChange goes out if the size differs. The viewer sees a short pause,
  never a disconnect.
- Clipboard helper ↔ broker: the broker passes the clipboard's control
  messages (17–20) through unchanged. The clipboard link (core
  `ClipLink`) runs in the helper, as the user. The broker itself never
  touches the clipboard.
- **Security:** each pipe has an explicit DACL: SYSTEM only for the screen
  helper, SYSTEM plus the signed-in user's SID for the clipboard helper.
  Each side checks that the process at the other end is Glass from the
  same install folder (`GetNamedPipeServerProcessId` /
  `GetNamedPipeClientProcessId` → image path), and the broker creates its
  pipes with `FILE_FLAG_FIRST_PIPE_INSTANCE`. This stops another program
  squatting the pipe name and feeding a SYSTEM helper fake input, or
  reading the clipboard.
- The in-app host (Glass.exe ▸ Allow connections, without the service) is
  unchanged: one process does everything as the user.

## Consequences
- Signing in, locking, Remote Desktop switches and UAC prompts become a short
  pause at most.
- The clipboard works the same with or without the service, because it
  always runs as the user. At the lock screen, before anyone signs in, there
  is no clipboard, which is correct.
- One more hop for frames and input, inside the PC: a pipe write and read
  (tens of µs). Frame buffers are reused, not allocated per frame.
- `docs/protocol.md` doesn't change: the viewer can't tell. The local pipe
  messages are internal and documented in `media_link.rs`.
- LATER(Mac unattended): the same split with a LaunchDaemon (connection) and a
  LaunchAgent per user (capture, input, clipboard), reusing `media_link`.
  Reaching the Mac's login window may not be possible (ADR 0008).
