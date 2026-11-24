# Test builds

Every push to `main` builds both apps in CI (artifacts `glass-win` and
`glass-mac` on the run page). At the end of each milestone a tag `mN-testK`
publishes a **prerelease** on the repo's Releases page with:

- `Glass-win-<tag>.zip`: `Glass.exe` (the app), `glass-cli.exe` (command line), FFmpeg DLLs, `host.cmd`, `connect.cmd`, `record-clips.cmd`, README
- `Glass-mac-<tag>.dmg`: `Glass.app` (ad-hoc signed, not notarized), README

The Mac doesn't need Rust or Xcode. First launch: right-click > Open, or
System Settings > Privacy & Security > Open Anyway.

## M5 test 24 (photos sharp, no blur cycle)
As test 23: from QUIETPC22 view QUIETPC, open photos and move around the
desktop and Start menu. Nothing should blur and sharpen by itself; photos
should be sharp a moment after they open. Then "done".

## M5 test 23 (photos sharpen)
Protocol v16 (unchanged). Update QUIETPC22 and QUIETPC (Glass and the
service).
1. From QUIETPC22, connect to QUIETPC. Open a few photos, each still for
   a second; in Fast and in Full colour.
2. Say whether they now sharpen fully, and how quickly.
3. Tell me "done".

## M5 test 22 (the virtual display on Glass's GPU; sharper images)
Protocol v16 (unchanged). Update QUIETPC22, QUIETPC (Glass and the
service) and the Mac; Share this Mac on.
1. From QUIETPC22, connect to QUIETPC. The first time may take ~6-10 s
   (QUIETPC's driver restarted onto its GPU). QUIETPC's monitor goes dark;
   QUIETPC22 shows it at its own 2560x1440.
2. Open a photo on QUIETPC; leave it still for a second: it should sharpen.
   Try Full colour (4:4:4) for photos.
3. Disconnect: QUIETPC's monitor comes back. Repeat once from the Mac.
4. Tell me "done".

## M5 test 21 (results between machines, and test 20)
Protocol v16: update all three machines, and the service on QUIETPC.
1. On the Mac, turn **Share this Mac** on (so it can answer for its logs).
2. Run test 20 below.
3. Then just tell me "done": I fetch QUIETPC's and the Mac's results from
   QUIETPC22 (`glass-cli results QUIETPC`, `… <Mac's name>`).

## M5 test 20 (the virtual display, fixed)
Protocol v15 (unchanged): update QUIETPC22, QUIETPC (Glass and the service).
1. From the Mac, connect to QUIETPC. The first time, expect up to ~6 s
   while QUIETPC's driver loads the Mac's size. QUIETPC's monitor goes
   dark; the Mac shows QUIETPC full screen, no black bars.
2. Use it a minute, disconnect: QUIETPC's monitor comes back.
3. Connect again: this time it should switch in about a second.
4. From QUIETPC22, connect to QUIETPC too (another size: QUIETPC22's
   2560x1440).
5. Tell me when done, and send QUIETPC's results (the "virtual display:"
   lines) and the Mac's.

## M5 test 19 (the virtual display in a session)
Protocol v15: update all three machines, and the service on QUIETPC.
1. On QUIETPC (it has the driver): in Glass's Settings press **Install
   virtual display** once more. It's already there; this time the
   installer also switches it off, so the second screen should go from
   Windows' Display settings.
2. Connect from the Mac to QUIETPC. Expected: QUIETPC's own monitor goes
   dark, and the Mac shows QUIETPC's desktop filling the whole Mac screen
   (no black bars) at the Mac's text size.
3. Use it for a few minutes (windows, Settings, the pool game). Then
   disconnect: QUIETPC's monitor should come back with the windows where
   they were.
4. Send both results. The PC's has a "virtual display:" line, saying what
   it used or why it couldn't.

## M5 test 18 (installing the virtual display, on QUIETPC22)
1. Unzip m5-test18 and start Glass.exe. Settings → Virtual display →
   **Install virtual display**. Allow the administrator prompt.
2. Note any Windows Security window (none / "trust SignPath Foundation" /
   a red "can't verify the publisher"), and what the black window says at
   the end; then press Enter.
3. Settings → System → Display: a second screen should be there. Say which
   sizes it offers (Display resolution).
4. Optional: **Remove virtual display** in Glass's Settings, and check the
   second screen goes.
Test 17 below still stands for sound and the Mac's refresh.

## M5 test 17 (sound from the Mac; the Mac at the PC's refresh)
Protocol v14, as before. Update QUIETPC22 and the Mac.
1. **The Mac viewing QUIETPC22** (pool, as before): a minute with the
   stats showing, two minutes hidden with the pointer away from the top.
   The bar should fit, with its status on a second line.
2. **QUIETPC22 viewing the Mac:** play something with sound on the Mac (a
   video or music). Expected: it plays from QUIETPC22's speakers, not the
   Mac's, in step with the picture. Move a window around: the PC's stats
   should show more than 60 fps (the Mac's screen at the PC's refresh).
3. Send all four results: the Mac's viewer and host results, and
   QUIETPC22's host and viewer results.

## M5 test 16 (the Keys menu on the Mac too)
Everything in test 15 below, plus: on the Mac viewing QUIETPC22, open the
bar and try "Keys": Start should open the PC's Start menu, Switch apps
Alt+Tab, Show desktop Win+D.

## M5 test 15 (lag on the Mac; no keyboard hook on Windows)
Protocol v14, as before. Update all three machines, and the service on QUIETPC.
1. The lag test: as test 14 below (a minute with the stats showing, then
   two minutes with them hidden and the pointer away from the top).
2. From QUIETPC22 view the Mac: type in a Mac app, use Ctrl+C / Ctrl+V,
   then the bar's "Keys" menu: Spotlight and Switch apps should open on
   the Mac. Windows Defender should leave Glass alone.
3. Send both results.

## M5 test 14 (lag: frames straight to the Mac's display)
Protocol v14, as before: update all three machines, and the service on QUIETPC.
1. Play pool for about a minute with the stats showing, as usual. The
   bar's handle now hides unless the pointer is near the top.
2. Hide the stats (⌃⌥⌘S) and play another two minutes, keeping the pointer
   away from the top. This is the part that counts.
3. Send both results. The Mac's "presented by" line now says how many
   frames went straight to the display; "present" and "input → screen"
   are the numbers to compare with test 13 (20.3 and 49.3 ms).
4. Say whether it feels less laggy.

## M5 test 13 (rate control comes back after the weak link)
Protocol v14, as before: update all three machines, and the service on QUIETPC.
1. As test 12: play, click "Weak Wi-Fi test" for about a minute, then off,
   and keep playing for at least another minute.
2. Expected: within about 30 s of turning it off, the Mac's "rate control
   (M5)" line says "no limit now" and the PC's frame rate goes back to
   twice the Mac's refresh (about 100+ fps sent).
3. Check the whole PC screen still shows in full screen (the chat panel on
   the right), before and after clicking "Weak Wi-Fi test", and with the
   control bar open.
4. Send both results.

## M5 test 12 (the whole PC screen on the Mac; rate control's climb)
Protocol v14, as before: update all three machines, and the service on QUIETPC.
1. Connect from the Mac: the whole PC screen shows in full screen,
   including the pool game's chat panel on the right, with thin black bars
   above and below. Leave and re-enter full screen (⌃⌥⌘F twice): the same.
2. As test 11: play, click "Weak Wi-Fi test" for about a minute, then off.
   Expected: the frame rate lowered only a few times (not 18), and far
   fewer keyframes asked for by the Mac (not 80).
3. Send both results.

## M5 test 11 (rate control: frame rate first)
Protocol v14, as before: update all three machines, and the service on QUIETPC.
1. 3D pool for about 30 s as normal.
2. Open the control bar (the handle at the top) and click "Weak Wi-Fi
   test": it turns orange and says "15 Mbps". Keep playing for about a
   minute. Expected: far fewer bitrate changes and keyframes than in
   test 10; the picture stays smooth.
3. Click it again (off). Within a few seconds it climbs back.
4. Send both results. On the PC, "rate control (M5)" now says how often
   the frame rate was lowered, and "IDRs" should be close to the bitrate
   changes, not 68.

## M5 test 10 (rate control, with the weak-link test mode)
Protocol v14: update all three machines, and the service on QUIETPC.
1. 3D pool for about 30 s as normal.
2. ⌃⌥⌘L on the Mac: "TEST: weak Wi-Fi, 15 Mbps" appears. Keep playing
   for about a minute. Expected: a moment of lag as the queue fills, then
   rate control backs off. The picture gets a little softer but stays
   smooth and quick.
3. ⌃⌥⌘L again (off). Within a few seconds it climbs back to full quality.
4. Send both results. The Mac's "test mode (weak link)" line has the
   times; "rate control (M5)" has the backoffs and the lowest limit; and
   "worst moments" shows whether anything stalled.

## M5 test 9 (rate control)
Protocol v14: update all three machines, and the service on QUIETPC.
1. The 3D pool test as before. On this link, "rate control (M5)" should
   say "no limit now" on the Mac and "no limit from the viewer" on the PC,
   or at most a few brief backoffs.
2. Then make the link worse:
   - start a big file copy, or a download on another machine;
   - or take the Mac further from the router.
   The picture should stay smooth and quick, perhaps a little softer. The
   Mac's line shows the limit and how often it backed off, and the PC's
   the bitrate it was set to.
3. Send both results.

## M5 test 8 (no false NACKs)
As M5 test 7. The Mac's "NACK:" line should show few asks (m5-test7: 1,142,
none needed) and "dup" in the hundreds, not tens of thousands. The PC's
"NACKs answered" drops the same way.

## M5 test 7 (pacing)
The same 3D pool test, especially the break.
1. Fewer stutters at the break? The Mac's "worst moments" and "Wi-Fi
   spikes" lines, against m5-test6.
2. The PC's results: "pacing (M5): N frames spread over X ms on average
   (longest Y)": X about 2 ms, Y at most about 8.
3. The picture's delay (capture -> screen) should be about the same as
   m5-test6 (36.5 ms): pacing adds at most a few ms to big frames.

## M5 test 6 (worst moments)
The same 3D pool test, starting from the menu (the static view) into the
live table. Note roughly when it felt off (seconds after connecting).
1. Send both results. The new lines are "worst moments (M5)":
   - on the Mac, the longest gaps on screen, each with its frame's size,
     network time and capture → screen;
   - on the PC, the longest capture pauses while moving, and the biggest
     frames with their encode time.
2. Everything from M5 test 5 too (60 fps shown, A/V).

## M5 test 5 (every refresh shown)
The same 3D pool test.
1. The Mac's results: "presented" close to 60 fps (m5-test4: 51.5).
2. "A/V (M4b)": the sound a few ms behind the picture (m5-test4: 18).
3. How does it feel against the uncapped m4b-test2?

## M5 test 4 (twice the refresh, the newest shown late in each refresh)
The same 3D pool test. Is it at least as smooth as m4b-test2 was?
1. The PC's results: "frame rate (M5): at most 120 fps, twice the viewer's
   screen"; "frames sent" up to ~120 while the balls move.
2. The Mac's results: "presented by: late in each refresh"; "present
   interval" p50 16.7 ms; "present" and "capture -> screen" lower than
   m5-test2's 32 and 54 ms.
3. Sound: "A/V" a few ms behind the picture.

## M5 test 3 (frames shown as soon as they're decoded)
The same 3D pool test, windowed and full screen (⌃⌥⌘F).
1. The Mac's results: "presented by: as decoded", and "present" plus
   "wait for refresh" lower than m5-test2's 32 + 9 ms. "capture -> screen"
   lower than 54 ms.
2. The PC's: "frames sent" about 60 fps or a little over.
3. Sound: "A/V" within a few ms behind the picture.
4. Note any stutters, and when (the break, scrolling, a still moment).

## M5 test 2 (60 fps for real; one refresh to show it)
The same 3D pool test.
1. Smooth? The PC's results: "frames sent" close to 60 fps while the game
   moves, and fewer "held back".
2. The Mac's results: "presented by: CAMetalDisplayLink (1 frame)",
   "present" p50 near 16 ms (was 32), and "capture -> screen" lower than
   54 ms. Try full screen too (⌃⌥⌘F), with the stats off once you've
   read them: full screen can skip compositing.
3. The cue click should still land with the hit ("A/V" a few ms behind).

## M5 test 1 (frames at the viewer's refresh)
The same 3D pool test: QUIETPC22 (Glass.exe sharing) viewed from the Mac.
1. The game should look smoother. Turn the stats on: "shown" should be
   close to 60 fps, and "capture → screen" lower than m4b-test2's 53 ms.
2. The sound's line should fall with the picture's, and stay a few ms
   behind it.
3. Send both results. The PC's has "frame rate (M5): at most 60 fps, the
   viewer's screen". The Mac's "present interval" p50 should be 16.7 ms,
   not 33.

## M4b test 3 (sound with the picture, fewer glitches)
As M4b test 2 (the 3D pool cue ball, the Mac's own speakers).
1. With the stats on, the "sound" line's total (X + the output's Y) should
   now be close to "picture p50", a few ms behind it.
2. Send the Mac viewer's results. "A/V (M4b)" should say a few ms
   behind, not ahead. "concealed" and "gaps" should be near 0 on a good
   link.

## M4b test 2 (sound in sync)
As M4b test 1. Sound should now be in step with the picture: lips with
speech, a game's sound with its action.
1. Turn the stats on (⌃⌥⌘S). The line "sound  X ms + the output's Y ms
   (picture p50 Z ms)" shows the delays live. X + Y should be close to Z,
   within about 20 ms.
2. Check a video with speech, and a game or anything with sharp sounds
   (clicks, gunshots).
3. If the output is Bluetooth (AirPods, a speaker), its delay (150–250 ms)
   shows as "the output's": try the Mac's own speakers or wired
   headphones to compare.
4. Send the Mac viewer's results ("sound (M4b)" and "A/V (M4b)" lines) and
   the PC host's.

## M4b test 1 (sound: PC → Mac)
Sound works from a Windows PC sharing with **Glass.exe** to the Mac
viewing it. The Glass service doesn't send sound yet (step 4), so on
QUIETPC switch Allow connections to Glass.exe sharing, or share from
QUIETPC22.
1. View the PC from the Mac. Play music or a video on the PC.
2. The Mac plays the sound. The PC's own speakers are silent during the
   session, and come back when it ends.
3. Listen for crackles, gaps or drift, including during a file copy and
   from a weaker Wi-Fi spot. Note whether the sound seems ahead of or
   behind the picture.
4. Send both results:
   - the Mac viewer's has "sound (M4b): N frames played … concealed …
     gaps … buffer … latency …";
   - the PC host's has "sound (M4b): N frames sent (M silent) | this PC's
     speakers muted during the session".

## M4 test 12 (Wi-Fi spikes named, the link reported)
1. View QUIETPC from QUIETPC22, then the Mac, for a few minutes each, on
   Wi-Fi, with something moving all the time (a video), so spikes show.
2. The viewer's results have two new lines: "Wi-Fi spikes (M4): N over
   8 ms above the typical X ms … | <likely cause>", and "Wi-Fi here:
   signal … | rate … | channel … | roams …". The host's results have its
   own "Wi-Fi here" line.
3. Send both. A Mac on the Wi-Fi often shows spikes "about every 1 s"
   (AWDL). A PC may show "about every 60 s" (a scan).

## M4 test 11 (Wi-Fi priority, the right adapter, radios kept awake)
Protocol v12: update all three machines, and the service on QUIETPC.
1. View QUIETPC from QUIETPC22, and the Mac, for a few minutes, including a
   file copy and a still screen.
2. Both results have a new line: "network (M4): … marked for Wi-Fi
   priority: yes | keep-awakes sent N". Send it if it says "no".
3. After the screen has been still for a while, the first frame of a
   change should arrive no slower than any other (compare the network p99
   and max with m4-test10).
4. If either PC has a VPN or Hyper-V/WSL installed: the Glass list on the
   other machine should show its real LAN address.

## M4 test 10 (no keyframe every 10 s; hitches measured)
1. View QUIETPC (or Glass.exe sharing) from QUIETPC22 for a few minutes
   with something moving. The host's results: "IDRs" should be about the
   keyframe requests plus one, not one per 10 s as before.
2. The viewer's FEC line ends with "hitches after a loss: N (avg, max
   ms)": how long the picture waited for a keyframe after a frame was lost.
   Send it, especially from a bad Wi-Fi spot.

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
