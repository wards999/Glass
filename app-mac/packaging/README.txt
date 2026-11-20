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
  overlay, and your mouse and keyboard control the PC.
    Ctrl+Option+Cmd+F   full screen / window
    Ctrl+Option+Cmd+S   hide / show stats
    Ctrl+Option+Cmd+W   disconnect (then "Copy results" and paste them back)
  Every other key goes to the PC: Cmd acts as Ctrl (Cmd+C copies), Ctrl as the
  Windows key, Option as Alt. Connection > PC-Style Modifiers swaps Cmd/Ctrl.
  macOS keeps Cmd+Tab, Cmd+Space and Mission Control for itself.

Recordings (M1): "Open Recording..." in the connect window, or double-click a .glsr.

Terminal use (same binary):
  /Applications/Glass.app/Contents/MacOS/GlassMac connect HOST[:PORT] [--seconds S]
  /Applications/Glass.app/Contents/MacOS/GlassMac play FILE.glsr [--bench]
