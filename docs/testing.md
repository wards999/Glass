# Test builds

Every push to `main` builds both apps in CI (artifacts `glass-win` and
`glass-mac` on the run page). At the end of each milestone a tag `mN-testK`
publishes a **prerelease** on the repo's Releases page with:

- `Glass-win-<tag>.zip`: `glass.exe`, FFmpeg DLLs, `host.cmd`, `record-clips.cmd`, README
- `Glass-mac-<tag>.dmg`: `Glass.app` (ad-hoc signed, not notarized), README

The Mac doesn't need Rust or Xcode. First launch: right-click > Open, or
System Settings > Privacy & Security > Open Anyway.

## M3 (mouse and keyboard)
Same as M2 (run `host.cmd` on the PC, connect from the Mac), then work the PC
from the Mac for a few minutes:
- type fast in Notepad; ⌘C / ⌘V / ⌘Z (⌘ is Ctrl on the PC); arrows and
  Backspace held down (they should repeat)
- click, double-click, drag-select text, right-click menus, drag windows
- scroll a long web page with the trackpad (both directions)
- switch away with ⌘Tab and back: nothing should be left held down on the PC
Watch "input → screen" in the overlay. Glass's own keys are now ⌃⌥⌘F (full
screen), ⌃⌥⌘S (stats) and ⌃⌥⌘W (disconnect); everything else goes to the PC.
Send back both results and how it felt (pointer, typing, scrolling).

## M2 (live stream)
Same network for both machines; wired Ethernet on the Mac if you have an
adapter (Wi-Fi works, but without FEC until M4 it shows more glitches).
1. Windows: unzip, double-click `host.cmd`. Allow glass.exe through Windows
   Firewall on Private networks if asked. Note the address it prints.
2. Mac: open `Glass.app`, allow Local Network access if asked, type the address,
   click **Connect**. The PC screen opens full screen with a stats overlay.
3. Use the PC for about a minute: scroll text, drag windows, play a video,
   minimise and restore. Watch the overlay (capture → screen latency, lost frames).
4. Press ⌘W on the Mac. Click **Copy results** and send them back, together
   with `results-host.txt` (or the text in the host.cmd window).
5. Optional: repeat with `host.cmd --chroma 444` and compare text sharpness.

## M1
1. Windows: unzip, double-click `record-clips.cmd`, and use the desktop while it
   records. Send back `results-win.txt`.
2. Copy `desktop-420.glsr` and `desktop-444.glsr` to the Mac.
3. Mac: open `Glass.app` and pick `desktop-420.glsr`. When playback ends, click
   **Copy results** and send them back. Repeat for `desktop-444.glsr`. While it
   plays, check that text is sharp and colours look right.

## Building locally
- Windows: `scripts/fetch-ffmpeg.ps1`, then `scripts/package-win.ps1` from a
  VS 2022 x64 developer prompt with the MSVC Rust toolchain.
- macOS: `scripts/package-mac.sh` (needs Rust + Xcode command-line tools).
