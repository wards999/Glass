# M3f — Windows service: unattended access

**Status: in progress.** The service (ADR 0015) is built and installs on
QUIETPC. The sharing it does works. The lock screen and UAC prompts are
still to be tested with a working keyboard.

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

## Still to do in M3f
- The lock screen, sign-in and UAC prompts through the service, now that
  typing works. Also End session from QUIETPC's window.
- The Win / Alt+Tab hook in Glass.exe.
- A "busy" refusal for a third device during a session (protocol v8, with M3e).
