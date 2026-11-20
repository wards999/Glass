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

Live viewer (M2):
  Run host.cmd on the PC; it shows the PC's address. Open Glass.app, type the
  address, click Connect. The PC's screen opens full screen with a stats
  overlay.
    Ctrl+Cmd+F   full screen / window
    Cmd+Shift+S  hide / show stats
    Cmd+W        disconnect (then "Copy results" and paste them back to Claude)

Recordings (M1): "Open Recording..." in the connect window, or double-click a .glsr.

Terminal use (same binary):
  /Applications/Glass.app/Contents/MacOS/GlassMac connect HOST[:PORT] [--seconds S]
  /Applications/Glass.app/Contents/MacOS/GlassMac play FILE.glsr [--bench]
