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
