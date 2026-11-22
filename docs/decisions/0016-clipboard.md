# 0016 — Clipboard sharing (M5c, step 1)

**Status:** accepted · **Date:** 2026-11-22

## Context
M5c covers clipboard and files. The user asked to start it ahead of the
M3f close-out. Step 1 is the clipboard: text and pictures, both ways,
between any pair (PC↔Mac, PC↔PC).

## Decision
- Items go over the session's TLS control connection as `ClipChunk`
  messages (protocol v8), not over UDP. They need to arrive whole, they are
  rare, and the connection is already encrypted and paired.
- Two kinds only: UTF-8 text (`\n` line endings) and PNG. Each side
  converts to and from its own formats: Windows CF_UNICODETEXT,
  "PNG" and CF_DIB; Mac public.utf8-plain-text, public.png and TIFF. Rich
  text (RTF/HTML) waits: plain text covers most pastes, and rich text needs
  per-app testing.
- Chunks of 60 KB are written one frame at a time from a helper thread, so
  pings, pointer shapes and the rest keep flowing while a large screenshot
  goes. Items over 16 MB aren't sent.
- The hot paths only swap an item through a slot. On Windows a clipboard
  thread per session watches the sequence number and reads, encodes and
  writes. On the Mac, main-thread pasteboard polling runs every 150 ms, with
  TIFF→PNG done off the main thread.
- No echo: each side remembers a hash of the last item it sent or put on
  the clipboard and doesn't send that again. This also stops loops when
  both ends share one clipboard (a loopback test on one PC).
- The viewer sends what is on its clipboard when the session starts, so
  copying and then connecting works. The host doesn't, so connecting doesn't
  overwrite the viewer's clipboard.
- A Clipboard switch (Shared/Off) in Settings on both apps, on by default.
  The Windows service learns it from `C:\ProgramData\Glass\control\no-clipboard`.

## Consequences
- Both ends must update together (v8), as usual.
- Text is preferred when an app offers both text and a picture
  (spreadsheet cells). A copied file currently sends its name as text;
  real files are step 2.
- LATER(M5c step 2): files, drag and drop, progress and pacing under M5's
  rate control.
