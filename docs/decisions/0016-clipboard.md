# 0016 — Clipboard sharing: text, pictures and files (M5c)

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

## Pictures and files go only when pasted (m5c-test3 feedback)
- The first file build sent everything when copied, which wastes the
  network on every copy. Now text still goes at once (small), but a picture
  or files are only **offered**: a few bytes (a size; names and sizes). The
  other side puts a promise on its clipboard and asks (`FileRequest`) only
  when an app pastes; the data comes then. This is how Remote Desktop
  works.
- Windows: pictures by delayed rendering (`WM_RENDERFORMAT`). Files as
  **virtual files** (`FileGroupDescriptorW` + `FileContents`, a COM data
  object), because Explorer reads a plain file list (CF_HDROP) the moment
  it's on the clipboard, to dim cut files: a promised CF_HDROP was fetched
  at once. With virtual files Explorer asks for contents only when
  pasting, and shows its own copy progress.
- Mac: lazy `NSPasteboardItem`s (a data provider). While a paste waits, the
  main run loop keeps turning, so the viewer keeps drawing.
- Promises are marked: a Glass marker so no Glass reads one back (that
  would fetch it), plus the standard "don't monitor" flags for clipboard
  history and clipboard managers (`ExcludeClipboardContentFromMonitorProcessing`,
  `org.nspasteboard.TransientType`).
- No size limit any more: copying is free. The paste waits for the data
  (about 2 minutes per GB at the current pacing), with progress in the
  viewer. LATER(M5c): stream each file's contents as it arrives instead of
  fetching the whole copy first.

## Files
- Pasted files land under a Glass folder first:
  - the viewer: `%TEMP%\Glass\clipboard-viewer` or the Mac's temp folder
  - the host: `clipboard-host`
  - the Windows service: `C:\ProgramData\Glass-clipboard`, which the
    signed-in user can read
  Once every byte is there, the clipboard holds them as files, so a normal
  paste copies them out. Windows marks them as a copy, so a paste never
  moves them.
- The file list goes first as a clipboard item (kind 3), then each file's
  bytes as `FileData`. TLS already guarantees integrity, so there's no
  separate hash. Sizes are checked and a broken transfer is dropped whole.
- One sender thread per session. A newer copy replaces anything not yet
  sent; a request for an older offer gets `FileCancel`. File data is paced at about
  60 Mbps so the picture keeps the network. LATER(M5): pace from the rate
  controller.
- At most 10 000 files and folders a copy. A copy that can't be listed
  (too many, or a file that can't be read, e.g. a cloud-only file) isn't
  offered; the results say why.
- Received paths are made safe: no `..` or absolute paths, and Windows'
  reserved names and characters are replaced. Older received copies are
  deleted when a new one arrives; the latest stays so it can still be pasted.
- Files that are already inside a Glass clipboard folder are never sent
  back. This avoids ping-pong, including in same-PC tests.
- The viewer shows progress ("Sending files to … 45%") in its notice box,
  even with the stats off.
- LATER: drag and drop onto the viewer.

## Consequences
- Both ends must update together (v8), as usual.
- Text is preferred when an app offers both text and a picture
  (spreadsheet cells).
- A receiver with the clipboard switched off still gets sent files it then
  ignores. LATER: tell the other side.
