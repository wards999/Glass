Glass for Mac (test build __VERSION__)
=====================================

1. Drag Glass.app to Applications (or anywhere).
2. First launch only: this test build isn't notarized by Apple, so macOS will
   block it. Either
     - right-click Glass.app > Open > Open, or
     - try to open it, then System Settings > Privacy & Security > "Open Anyway", or
     - in Terminal:  xattr -dr com.apple.quarantine /Applications/Glass.app
3. Open Glass.app and pick a .glsr recording (or double-click a .glsr file).
   It runs a decode benchmark, then plays the clip in a window, then shows the
   results. Click "Copy results" and paste them back to Claude.

Terminal use (same binary):
  /Applications/Glass.app/Contents/MacOS/GlassMac play FILE.glsr [--bench]
