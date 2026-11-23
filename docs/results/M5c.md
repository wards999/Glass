# M5c — Clipboard and files (both directions)

**Status: closed (2026-11-23), with the open items below.** Protocol v9,
ADR 0016. Built ahead of M4/M5 at the user's request.

## What works
- **Text** both ways, at once. m5c-test2 onwards: line breaks, emoji and
  accented letters checked by the user.
- **Pictures** are offered and sent only when pasted. Windows uses delayed
  rendering, and the Mac a lazy pasteboard item.
- **Files and folders** are offered and sent only when pasted, with no size
  limit. On Windows they arrive as virtual files streamed into Explorer's own
  copy window; on the Mac as file URLs. Received files are made safe (no
  `..`, Windows-safe names).
- **With the Windows service** the clipboard runs as the signed-in user
  (ADR 0017), so it behaves as without the service.
- **Speed adapts to the picture (v9).** The viewer steers file speed every
  0.25 s from the video frames' own network time, loss and the round trip.

## Measured
| test | what | result |
|---|---|---|
| m5c-test3 (user) | 1 GB, QUIETPC ↔ QUIETPC22 | 2 min 20 s, fixed 7.5 MB/s; Remote Desktop: 2 min 7 s |
| m5c-test9 (user) | the same with adaptive speed | "a lot faster" |
| m5c-test9 (user) | the picture during the copy | network p99 25.9 ms (8.5 without a copy); capture → screen p99 41 ms |
| local, m5c-test9 code | 300 MB, one PC | 8.5 s (about 35 MB/s), byte-identical |
| local, m5c-test10 code | 300 MB through the service path | 7.8 s, identical; video network p99 1.81 ms; hop max 1.58 ms |
| local | copy without paste | 0 bytes fetched (files and pictures) |

m5c-test10 reacts to the picture sooner: the frames' own network time over
baseline by 5 ms halves the speed. Its effect on a real network is in
m5c-test12's list and not reported yet.

## Carried forward (open)
- The Mac clipboard (pasting on demand, files Mac ↔ PC). It has never run on
  a Mac. It's in m5c-test12's list.
- The 1 GB copy time and picture smoothness with m5c-test10's steering.
- One copy on QUIETPC22 was "not offered: a file couldn't be read"
  (m5c-test9). Which file is unknown: likely a OneDrive cloud-only file.
- Progress while pasting on the Mac (file promises), drag and drop onto the
  viewer, rich text (RTF/HTML).
