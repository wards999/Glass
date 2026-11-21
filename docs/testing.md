# Test builds

Every push to `main` builds both apps in CI (artifacts `glass-win` and
`glass-mac` on the run page). At the end of each milestone a tag `mN-testK`
publishes a **prerelease** on the repo's Releases page with:

- `Glass-win-<tag>.zip`: `glass.exe`, FFmpeg DLLs, `host.cmd`, `record-clips.cmd`, README
- `Glass-mac-<tag>.dmg`: `Glass.app` (ad-hoc signed, not notarized), README

The Mac doesn't need Rust or Xcode. First launch: right-click > Open, or
System Settings > Privacy & Security > Open Anyway.

## M3d test 2 (control bar both ways, any pair)
Use the zip and dmg from the same test build (protocol v6).
1. **PC views the Mac** (connect.cmd):
   - Open the bar from the top edge.
   - Views and both colour modes should work.
   - Keys: Ctrl acts as ⌘.
   - Drag windows for a minute or two in each colour mode; the lag must stay
     steady.
2. **Mac views the PC** (Glass on the Mac ▸ Connect; host.cmd on the PC):
   - Move the pointer to the top edge to open the bar.
   - The fit views are greyed out (a PC can only show its own screen).
   - Fast ↔ Full colour should switch, and the PC window says "Viewer
     switched to …".
   - Check that ⌘C/⌘V still work as before.
3. **PC views PC** (the second PC): host.cmd on one, connect.cmd on the
   other. Ctrl stays Ctrl (Ctrl+C copies), the Win key opens Start on the
   host, and the views are greyed out.
4. **No loops:** while the PC views the Mac, open Glass on the Mac (in the
   remote view) and try to connect back to the PC. It must refuse with "that
   would make a loop". After disconnecting, Mac → PC works again.
5. Send all the reports: connect.cmd text, host.cmd text, and the Mac's
   Show Results.

## M3c (sharp text, view modes, 4:4:4)
Use the zip and dmg from the same test build (protocol v5).
1. **Mac:** install the new Glass.app, open it, and click **Share This Mac…**.
   The permissions should still be granted; if macOS asks again, say so.
2. **PC:** extract the zip, run `connect.cmd`, and type the Mac's address. It
   starts on the Mac's own screen, as in M3b. The window should say full
   colour (4:4:4) is on.
3. **On the Mac** (from the PC), click **Clarity Test Card** in the Share
   window.
4. Press **Ctrl+Alt+Shift+R** to step through the four views:
   - the Mac's screen
   - fit, like now
   - fit, more space
   - fit, larger
   The Mac's own screen mirrors the "fit" views, and returns to normal when
   you go back to the Mac's screen or disconnect. In each view, look at the
   test card (small coloured text, 1-pixel stripes) and at normal apps, and
   note which view you'd keep.
5. Ctrl+Alt+Shift+Q to disconnect. Send:
   - the connect.cmd text, and
   - the Mac's results (Show Results ▸ Copy). They include sharpness (PSNR)
     from the test card.
   Also say which view looked best, and whether the Mac's screen came back
   to normal afterwards.

## M3b (the PC views and controls the Mac)
Use the zip and dmg from the same test build.
1. **Mac:** install the new Glass.app over the old one (Applications), open
   it, and click **Share This Mac…**. macOS asks for Screen Recording and
   Accessibility. Allow both in System Settings (the window has buttons that
   open the right page), then quit Glass (⌘Q) and open it again.
   From this build on, the app is signed with the same certificate every
   time, so you won't need to do this again for later test builds.
   The window shows the Mac's address and "Waiting for the PC…".
2. **PC:** unzip, double-click **connect.cmd**, and type the Mac's address.
   The first time, the Mac shows a 6-digit PIN: type it in the connect.cmd
   window. The Mac opens full screen on the PC.
3. Use the Mac from the PC for a few minutes:
   - Type in TextEdit or Notes. Ctrl+C / Ctrl+V / Ctrl+Z copy, paste and
     undo (Ctrl acts as ⌘).
   - Use the arrows and hold Backspace (they should repeat).
   - Click, double-click a word, drag-select, right-click, drag windows.
   - Scroll a long page with the mouse wheel.
   - Alt+Tab away from the viewer and back: nothing should stay held down
     on the Mac.
4. Press **Ctrl+Alt+Shift+Q** to disconnect. Send:
   - the connect.cmd text (also saved in `results-viewer.txt`), and
   - the Mac's results (**Copy Results** in the Share This Mac window).
   Also say how it felt: pointer, typing, scrolling, and text sharpness.
5. **The Mac's 4:4:4 check:** in Terminal run
   `/Applications/Glass.app/Contents/MacOS/GlassMac check-444` and send the
   two lines it prints. They decide how M3c keeps text sharp.
6. Optional: Ctrl+Alt+Shift+F switches between window and full screen.

## M6 (pairing and encryption)
Use the zip and dmg from the same test build (v4 can't talk to older builds).
1. Windows: run `host.cmd`. It prints this PC's name and id and says no device
   is paired yet. Allow glass.exe through the firewall if asked.
2. Mac: open Glass.app. The PC should appear under "PCs running Glass on this
   network" (if it doesn't, type the address; say so in the results). Pick it,
   click **Connect**. Glass asks to pair; click **Pair**.
3. The host.cmd window shows a 6-digit PIN. First type a wrong PIN on the Mac:
   it should say wrong PIN and offer Try Again (the PC says 2 tries left).
   Then type the right one: the stream opens.
4. Use the PC from the Mac for a minute as in M3, then ⌃⌥⌘W, **Copy
   results**, and send them with the host.cmd text. Compare with M3: the
   numbers should be the same (encryption costs about 0.3 µs per packet).
5. Connect again: no PIN this time. The list should show "✓ paired".
6. On the PC (a second Command Prompt in the Glass folder): `glass devices`
   lists the Mac. Then `glass forget "<Mac name>"`, and connect from the Mac
   again: it must ask to pair again (and the PC says it refused an unpaired
   device). Pair again so you're left paired.
7. Mac: Connection ▸ Paired Devices shows the PC; Forget works the same way
   (then pair again).

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
Same network for both machines. Wi-Fi is the main setup (5 or 6 GHz if the
router has it); a wired run is useful only as a comparison.
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
