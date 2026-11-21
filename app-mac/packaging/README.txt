Glass for Mac (test build __VERSION__)
=====================================

1. Drag Glass.app to Applications (or anywhere).
2. First launch only: this test build isn't notarized by Apple, so macOS will
   block it. Either
     - right-click Glass.app > Open > Open, or
     - try to open it, then System Settings > Privacy & Security > "Open Anyway", or
     - in Terminal:  xattr -dr com.apple.quarantine /Applications/Glass.app
3. If macOS asks "Allow Glass to find devices on local networks?", click Allow
   (needed to reach the PC). If you clicked Don't Allow, turn it on in
   System Settings > Privacy & Security > Local Network.

Live viewer:
  Run host.cmd on the PC. Open Glass.app, pick the PC from the list (or type
  the address host.cmd shows), click Connect. The first time, Glass asks to
  pair: type the 6-digit PIN shown in the host.cmd window. After that it
  connects straight away. Only paired devices can connect, and everything is
  encrypted. The PC's screen opens full screen with a stats overlay, and your
  mouse and keyboard control the PC.
  Connection > Paired Devices lists and forgets paired PCs. This Mac's Glass
  identity lives in ~/Library/Application Support/Glass (readable only by you).
    Ctrl+Option+Cmd+F   full screen / window
    Ctrl+Option+Cmd+S   hide / show stats
    Ctrl+Option+Cmd+W   disconnect (then "Copy results" and paste them back)
    Ctrl+Option+Cmd+Q   quit Glass (Cmd+Q goes to the PC while viewing)
  Every other key goes to the PC: Cmd acts as Ctrl (Cmd+C copies), Ctrl as the
  Windows key, Option as Alt. Connection > PC-Style Modifiers swaps Cmd/Ctrl.
  macOS keeps Cmd+Tab, Cmd+Space and Mission Control for itself.

Share this Mac (another Mac or PC views and controls this Mac):
  Turn on the "Share this Mac" switch at the bottom of the Glass window. The
  card opens up and shows this Mac's address, any missing permission with a
  link to it, and who is connected:
    - Screen Recording (to show this screen): allow it, then quit and reopen Glass.
    - Accessibility (for the other device's mouse and keyboard).
  Test builds are signed with the same certificate each time, so these
  permissions stay granted across builds.
  The other device sees this Mac in its Glass list. The first time, the card
  shows a 6-digit pairing code to type there. After a session, "Show Results"
  copies its numbers. Turn the switch off to stop sharing.
  Check for sharp text later (M3c):  GlassMac check-444  (see below).

Recordings (M1): "Open Recording..." in the connect window, or double-click a .glsr.

Terminal use (same binary):
  /Applications/Glass.app/Contents/MacOS/GlassMac connect HOST[:PORT] [--pin PIN] [--seconds S]
  /Applications/Glass.app/Contents/MacOS/GlassMac play FILE.glsr [--bench]
  /Applications/Glass.app/Contents/MacOS/GlassMac host [--pin PIN] [--port P] [--seconds S]
  /Applications/Glass.app/Contents/MacOS/GlassMac check-444
