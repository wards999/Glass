# M3f — Windows service: unattended access

**Status: closed (2026-11-23), with the open items below.** The service
(ADR 0015) installs and runs on QUIETPC. Since m5c-test7 it keeps the viewer's
connection while helpers do the work (ADR 0017), so signing in or Remote
Desktop no longer drops the viewer. The user moved on to M4; the open items
are carried forward.

## Found on the way (PC → PC, m3d-test18 … m3f-test7)

- **Remote Desktop:** QUIETPC sent 0 frames to QUIETPC22 because Glass was
  running inside a Windows Remote Desktop session. No capture API can see that
  session, and RD keeps the PC's own screen locked meanwhile. The service now
  shares the console session instead. The window spots Remote Desktop and
  offers "Move it to the PC's screen" (tscon).
- **Resolution:** a Windows host sends its screen as it is (QUIETPC's
  1280×1024 monitor, scaled up on a 2560×1440 viewer). Resolution matching now
  switches to the best mode the display offers for the session. m3f-test2
  picked 1280×720 for a 16:9 viewer, which was worse; since m3f-test3 it only
  switches to more pixels. A 1280×1024 monitor has nothing better unless
  NVIDIA DSR or a dummy plug adds modes. Exact sizes need the M7 virtual
  display.
- **Keyboard from the Windows viewer:** keys never left the viewer.
  - QUIETPC's report: 0 keys, 12 clicks, 25 scrolls, 1613 moves.
  - QUIETPC22 → Mac failed the same way.
  - The low-level keyboard hook saw 3 presses and then 0 (m3f-test3,
    m3f-test5): Windows had stopped calling it inside Glass.exe.
  - Since m3f-test7 the viewer also takes keys as window messages:
    "5 presses via the window, 0 seen by the hook, 5 sent". Keys reach the Mac.
  - Still open: Win, Alt+Tab and Ctrl+Esc need the hook, and it still sees
    nothing in Glass.exe.
- **Session bar:** both windows show who's connected, with Disconnect / End
  session, and block other connections for the session (m3f-test5/7).

## m3f-test7, QUIETPC22 → MacBook Air (Wi-Fi, 85 s)

| | p50 | p99 |
|---|---|---|
| capture → screen | 22.2 ms | 48.7 ms |
| input → screen | 39.8 ms | 80.5 ms |

0 lost frames. The session switched 4:2:0 → 4:4:4 → 4:2:0 from the bar.

## The service keeps the connection (ADR 0017), QUIETPC22 → QUIETPC

Signing in at QUIETPC's lock screen (PIN typed in the viewer) used to drop the
session and reconnect about 5 s later. With the broker and helpers:

| build | session switches, viewer kept connected | picture paused | helper → service hop p50 / p99 / max |
|---|---|---|---|
| m5c-test8 (463 s) | 1 | 0.37 s | 0.14 / 0.53 / 2.61 ms |
| m5c-test9 (231 s) | 1 | 0.24 s | 0.10 / 0.52 / 58.4 ms |

- The 58 ms max was a video frame waiting behind a file chunk in the broker's
  loop. Fixed in m5c-test10: clipboard data has its own path. A local test
  during a 300 MB copy then measured hop max 1.58 ms.
- On one PC, two rounds at about 140 fps: capture → screen p50 was
  10.38 / 10.35 ms in-app and 10.47 / 10.38 ms through the broker, and p99
  16.80 / 16.82 vs 16.80 / 16.42 ms. The hop doesn't show.
- Clipboard with the service: files crossed both ways (m5c-test8: "1 copies
  offered, 1 pasted on the viewer, 3 pasted here").

## Carried forward (open)
- UAC prompts, Remote Desktop take-over and give-back, and End session
  through the new service. In m5c-test12's list, not yet reported.
- The Win / Alt+Tab hook in Glass.exe.
- A "busy" refusal for a third device during a session.
- Ctrl+Alt+Del (SendSAS) for PCs that require it.
