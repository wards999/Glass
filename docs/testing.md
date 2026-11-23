# Test builds

Every push to `main` builds both apps in CI (artifacts `glass-win` and
`glass-mac` on the run page). At the end of each milestone a tag `mN-testK`
publishes a **prerelease** on the repo's Releases page with:

- `Glass-win-<tag>.zip`: `Glass.exe` (the app), `glass-cli.exe` (command line), FFmpeg DLLs, `host.cmd`, `connect.cmd`, `record-clips.cmd`, README
- `Glass-mac-<tag>.dmg`: `Glass.app` (ad-hoc signed, not notarized), README

The Mac doesn't need Rust or Xcode. First launch: right-click > Open, or
System Settings > Privacy & Security > Open Anyway.

## M4 test 9 (Mac session logs, a tidier Mac window, paired devices)
1. On the Mac, share and view for over 10 s. `~/Library/Logs/Glass` gets a
   `host-…txt` / `viewer-…txt` that updates every 10 s while the session
   runs ("still running after N s") and holds the final results at the end.
   The newest 20 are kept. Console.app shows them under Log Reports.
2. The Mac's sharing card no longer has Show Results / Clarity Test Card.
   Settings has Session results and a new "Clarity test card" button.
3. Settings > Paired devices on QUIETPC22 no longer lists QUIETPC22
   itself, and each row shows the device's id. Two rows with one name are
   an old identity: Forget the one that's not current (the Mac's Settings
   shows "this Mac · id …" at the bottom; on a PC, the last line of
   Settings).

## M4 test 8 (a progress panel while the Mac pastes files)
1. View the Mac from QUIETPC22. Copy a big file (1 GB) on QUIETPC22, and
   paste it in Finder on the Mac (through the viewer, or on the Mac itself).
2. Finder still beachballs until the file is all there (macOS can't do
   better for Cmd+V), but a panel now appears at the top of the Mac's
   screen: "Pasting from “QUIETPC22”…", a bar, MB, %, speed, time left.
   It shows in the viewer's picture too, and closes when the paste is done.
3. The same with the Mac viewing QUIETPC: copy on QUIETPC, paste on the Mac.

## M4 test 7 (Stop sharing keeps Allow connections on)
1. View the Mac from QUIETPC22, then on the Mac press Stop Sharing in the
   Glass window. The session ends, QUIETPC22 doesn't reconnect by itself,
   and Allow Connections stays on: connecting again works without
   switching anything back on.
2. The same with QUIETPC as host (the service), and with Glass.exe sharing.
3. The NACK line now counts a frame as saved only if a resent packet
   completed it.

## M4 test 6 (NACK: lost packets asked for again)
Protocol v11: update all three machines, and the service on QUIETPC.
1. As M4 test 1: a few minutes on Wi-Fi with ordinary use and a video,
   QUIETPC22 viewing QUIETPC, then the Mac. Make the Wi-Fi worse if you can.
2. The picture should hitch less: a lost packet is now asked for again
   (one round trip, a few ms) instead of waiting for a whole new keyframe.
3. Send the viewer results. The FEC line now ends with
   `NACK: N asks, R packets resent, F frames saved`. `lost frames` and
   `keyframe requests` should be lower than before.
4. The host's FEC line ends with `NACKs answered N (R packets resent)`.

## M4 test 3 (FEC, the loss fix, logs)
If Windows Defender removes Glass.exe again: Windows Security > Virus &
threat protection > Protection history > the entry > Actions > Allow on
device. It's unsigned (ADR 0008). Each build is new to Defender.
1. As M4 test 1 below, including a YouTube video from QUIETPC on QUIETPC22.
2. Glass.exe > right-click > Properties > Details now says "Glass remote
   desktop", version 0.0.1 (m4-test3).
3. While a session runs, `%LOCALAPPDATA%\Glass\logs` gets a `viewer-…txt`
   (and `host-…txt` when Glass.exe shares) that updates every 10 s.
   Settings > Session results shows the newest one after Glass restarts.

## M4 test 1 (FEC: lost packets rebuilt)
Protocol v10: update all three machines, and the service on QUIETPC.
1. View QUIETPC from QUIETPC22, and the Mac from QUIETPC22, for a few
   minutes each, on Wi-Fi, doing ordinary things (scrolling, dragging
   windows, a video).
2. If you can, make the Wi-Fi worse: walk the laptop further away, or run a
   big download on the same network.
3. Send the viewer results (Settings > Session results). The new line is
   `FEC (M4): rebuilt N lost packets in M frames | packet loss seen X%`.
   Compare `lost frames` and `keyframe requests` with before: m5c-test9 had
   2 lost frames and 6 keyframe requests in 231 s.
4. The host's results have `FEC (M4): N parity packets (X% of packets)`.

## M3f test 2 (the service keeps the connection, ADR 0017)
The service changed a lot. Update it on QUIETPC first: from the new zip, run
`glass-cli service install` as administrator, or use Settings > Unattended
access. Update QUIETPC22 and the Mac too.

From QUIETPC22 (and then from the Mac), view QUIETPC:
1. **Sign in without a drop:** lock QUIETPC (Win+L in the viewer), then sign
   in with your PIN in the viewer. The picture may pause for a moment, but
   the viewer stays connected: no window closing, no reconnecting.
2. **Remote Desktop:** while viewing, connect to QUIETPC with Remote Desktop,
   then disconnect it (or use "Move it to the PC's screen"). The Glass viewer
   stays connected throughout. It shows the lock screen while Remote
   Desktop has the session.
3. **UAC:** run something as administrator on QUIETPC. Answer the prompt in
   the viewer.
4. **Clipboard with the service:** text, a picture, and files, both ways
   (copy on QUIETPC, paste on QUIETPC22, and the other way round). Files
   are pasted with Explorer's own copy window.
5. **At the lock screen:** before anyone signs in, the clipboard doesn't
   cross (nobody's clipboard to use). After signing in it works again.
6. **Stop sharing and End session** in the Glass window on QUIETPC still work.
7. Send back the viewer results, and from QUIETPC
   `C:\ProgramData\Glass\agent-results.txt` (last two sessions) and the end of
   `C:\ProgramData\Glass\service.log`. The results have a line "session
   switches with the viewer kept connected: N (picture paused for …)".

## M5c test 4 (clipboard: text, pictures and files, both ways)
Update **both** machines first. This is protocol v8, so an old build on either
side refuses to connect. On the PC with the service, run
`glass-cli service install` from the new zip as administrator (it updates the
installed service), or use Settings > Unattended access.
Settings > **Clipboard** should say **Shared** on both.

With a session on (Mac viewing PC, then PC viewing Mac; PC↔PC too if you can):
1. **Copy, then connect:** copy some text on the viewer, then connect. Paste
   it on the host (in the viewer window).
2. **Viewer → host text:** copy a few lines on the viewer (several lines, an
   emoji, an accented letter). Paste them into Notepad / TextEdit on the host.
   The line breaks should be right.
3. **Host → viewer text:** copy text on the host (in the viewer window), then
   paste it into an app on the viewer, outside Glass.
4. **Pictures both ways:** take a screenshot to the clipboard (PC:
   Win+Shift+S; Mac: Ctrl+Cmd+Shift+4) and paste it on the other side (Paint,
   Word, Preview > New from Clipboard, Mail). Also try an image copied from a
   web page.
5. **Big picture while moving:** copy a full-screen screenshot and at once
   drag a window around in the viewer. The picture must not stutter while the
   clipboard goes across.
6. **No echo:** after each paste, the clipboard keeps what you pasted. It
   shouldn't change back or flicker.
7. **Copying costs nothing:** copy a big file (1 GB or so) on either side
   and don't paste it. Nothing is sent: no progress line, and the viewer's
   network figure in the stats doesn't jump.
8. **Files, viewer → host:** in Explorer / Finder on the viewer, copy two
   files and a folder with a subfolder. On the host (in the viewer window),
   paste into a folder there. The files come now: a progress line ("Sending
   files to …") and, on Windows, Explorer's own copy window. Check that
   everything arrived and opens.
9. **Files, host → viewer:** copy files on the host, then paste them into a
   folder on the viewer outside Glass.
10. **Big paste while moving:** paste about 500 MB and drag a window around in
    the viewer while it comes. The picture must stay smooth.
11. **Paste twice:** paste the same copy into a second folder. The second one
    is quick (it's already here).
12. **The service (QUIETPC):** text, pictures and files both ways. If files
    copied on QUIETPC still don't show up on the other side, send its results:
    the `clipboard:` line says why ("not offered").
13. **Off:** set Clipboard to **Off** on the viewer and reconnect. Copying on
    either side no longer crosses. Set it back to **Shared**.
14. Send back the viewer and host results (Copy button). Each has a
    `clipboard:` line counting text, pictures and file copies each way.

## M3f test 1 (the Windows service: unattended access)
On the PC to be reached from far away (QUIETPC), from the extracted zip:
1. Open **Glass.exe** > gear > **Unattended access** > **Install service …**.
   Windows asks for administrator rights; a window shows what it did, then
   press Enter. (Or run `glass-cli service install` as administrator.)
2. The share card now says **Always on** with a **Service** tag.
3. From the other PC (or the Mac), connect to it. You should see its screen.
4. **Lock it** (Win+L on the remote view, or lock it at the PC). The viewer
   should show the lock screen; click and type the password in the viewer to
   sign in.
5. Open something that asks for admin rights (e.g. right-click Command Prompt
   > Run as administrator): the UAC prompt should show and answer in the viewer.
6. Through Windows Remote Desktop: connect to QUIETPC with Remote Desktop, then
   view it with Glass: you see its own screen (locked while Remote Desktop is
   connected) and can sign in there (this ends the Remote Desktop session).
7. **Resolution:** with NVIDIA DSR on (or a bigger monitor), QUIETPC should
   switch to a sharper mode while viewed and back afterwards.
8. Send back the viewer results, `C:\ProgramData\Glass\service.log`, and
   anything odd., and `C:\ProgramData\Glass\agent-results.txt`. Remove it again any time from the same Settings section.

## M3d test 6 (the redesigned window; replaces tests 4 and 5)
Use the zip and dmg from the same test build (protocol v7). Extract the zip
on both PCs.
1. **Look:** open Glass on the PC and the Mac. Devices on the network are
   rows ("Ready" = paired, "Pair" = needs a code). With none found, a
   "Looking for devices" card shows instead.
2. **Theme:** the moon/sun button switches light/dark; the gear opens
   Settings (System/Light/Dark, colour, paired devices with Forget). Restart
   each app: the choice is kept.
3. **Connect:** click a device row (hover shows Connect). The code dialog
   appears the first time. Use the bar, then disconnect: results open with
   Copy.
4. **Share:** flip the "Share this PC" / "Share this Mac" switch. Connect from
   the other machine; on the PC the pairing code shows in large tiles. Close
   the PC window while sharing: Glass stays in the tray.
5. **No loops:** while the PC views the Mac, connect from the Mac (in the
   remote view) back to the PC. It must refuse.
6. **PC to PC** with the second PC, if handy.
7. Send the results, screenshots of the Mac window (light and dark), and
   anything that looked or felt wrong.

## M3d test 4 (the Glass window, machine list, no loops)
Use the zip and dmg from the same test build (protocol v7). Extract the zip
on both PCs.
1. **PC:** double-click **Glass.exe**. The Glass window opens (no console),
   with the icon. The Mac (sharing) and the other PC (if sharing) appear in
   the list.
2. **Connect** by double-clicking the Mac in the list. The PIN dialog
   appears the first time. Use the bar, then disconnect: the results appear.
3. **Share this PC** in the window. Connect from the Mac (its list now says
   "(PC)"). Close the PC window while sharing: Glass stays in the tray.
4. **No loops:** while the PC views the Mac, on the Mac (in the remote view)
   connect back to the PC. It must refuse with "that would make a loop".
5. **PC → PC** with the second PC: Share this PC on one, connect from the
   other's list.
6. Send the results (copied from the windows) and anything that felt wrong.

## M3d test 2 (control bar both ways, any pair)
Use the zip and dmg from the same test build (protocol v6).
1. **PC views the Mac** (connect.cmd):
   - Open the bar from the handle at the top.
   - Views and both colour modes should work.
   - Keys: Ctrl acts as ⌘.
   - Drag windows for a minute or two in each colour mode; the lag must stay
     steady.
2. **Mac views the PC** (Glass on the Mac ▸ Connect; host.cmd on the PC):
   - Click the handle at the top to open the bar.
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
