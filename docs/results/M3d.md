# M3d — Apps on both ends: results

**Status: in progress.** First piece built: the viewer's drop-down control bar
on Windows (`m3d-test1`). Still to do: the Windows app with tray, window and
settings; the Mac menu-bar icon; the Mac viewer's bar; the app icon.

## Control bar (Windows viewer)
- Move the pointer to the top edge of the viewer and the bar drops down.
  It hides about 0.7 s after the pointer leaves, unless pinned.
- It has:
  - View: Fit · More space · Larger · Mac's screen (current one pressed)
  - Colour: Fast (4:2:0) · Full colour (4:4:4) (greyed out if this PC can't
    decode 4:4:4)
  - Full screen / Window, Pin, Disconnect
  - Status: host, size, colour, round trip
- It never takes the keyboard (`WS_EX_NOACTIVATE`), so keys still go to the
  host, and clicks on it never reach the host.
- The keyboard shortcuts remain.
- Checked here against `glass serve` in a window; captured with
  `GLASS_SHOW_BAR=1 GLASS_DEBUG_SHOT=file.bmp`:

![control bar](img/m3d-bar-local.png)

## Also in m3d-test1 (from M3c)
- The Mac host keeps only the newest frame. To check: dragging must not get
  laggier over time.
- 4:2:0 by default; 4:4:4 from the bar or Ctrl+Alt+Shift+C.

## m3d-test2 (built 2026-11-21)
- **Protocol v6:** `Welcome` carries the host's OS and capabilities.
  - The viewer picks keys by itself: Ctrl↔⌘ only across Mac/PC, as
    labelled between two PCs or two Macs. `--keys` overrides.
  - The bars grey out what the host can't do.
- **The PC host switches 4:2:0 ↔ 4:4:4 mid-session** (new NVENC encoder,
  StreamChange, IDR) and follows the viewer's preference from `Hello`.
- **The Mac viewer:**
  - It has the same drop-down bar: views, colour, full screen, stats, PC
    keys, pin, disconnect, status.
  - It handles StreamChange: a new decoder, picture shape and pointer scale.
  - It sends its screen size, so a Mac host can fit it (Mac→Mac).
  - It remembers the view per host and the colour choice.
- **CI:** a Mac→Mac run on the runner; the packaged app hosts a test pattern
  and its own viewer connects.
- **Exodus:** its password window doesn't appear in the stream. The app
  marks that window "no screen capture", and macOS leaves it out of every
  capture (screenshots, Zoom, Glass). It is still in front on the Mac, so
  typing reaches it. That's by design; Glass can't and shouldn't override it.

## m3d-test3: no connection loops (protocol v7)
- Each viewer's `Hello` lists who currently sees its machine, transitively:
  its viewers and theirs, up to 8.
  - A host process records this in `seen-by.txt` and refreshes it every 3 s.
  - Records older than 10 s are ignored, so a crashed host can't block
    anything.
- A host refuses (reject 7 "loop") when the viewer is itself, or when the
  host is on that list. That covers A↔B and longer chains like A→B→C→A.
- **Test:** `connection_loops_are_refused` checks:
  - PC views Mac, then Mac → PC: refused, and the PC host reports it.
  - PC → itself: refused.
  - After the PC disconnects, Mac → PC works.
- Also fixed: the Mac viewer's report showed "0x0" after the session closed.

