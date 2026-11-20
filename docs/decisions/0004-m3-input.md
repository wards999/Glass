# 0004 — M3 input and pointer

**Status:** accepted (M3) · **Date:** 2026-11-20

## Context
M3 makes PC → Mac usable: mouse and keyboard back to the host, pointer drawn
locally. Input is tiny but every event matters (a lost key-up is a stuck key),
and it must not add latency.

## Decisions
1. **UDP, events + state in every datagram** (`core/src/input.rs`). Recent
   events (300 ms, ≤ 16) are repeated in every datagram and applied once by
   sequence number; the current state (pointer, buttons, held keys) is
   reconciled afterwards. A lost datagram costs nothing; even losing every copy
   of an event can't leave a key or button stuck. A test drives 2000 random
   key events through 20% loss: nothing stuck, ≥ 99% delivered exactly once.
   TCP would be simpler but puts every keystroke behind any retransmit.
2. **Physical keys as USB HID usages.** The host's keyboard layout decides the
   character, which is right when both machines use the same layout. Mac
   ISO keyboards report two keys swapped compared with a PC; the viewer
   detects ISO and maps by position. LATER: text/IME input for other layouts.
3. **Modifier rule on the viewer:** default ⌘ → Ctrl, ⌃ → Win, ⌥ → Alt so Mac
   shortcuts keep working (⌘C copies); a menu toggle gives ⌘ → Win, ⌃ → Ctrl.
   The host only ever sees HID keys.
4. **Glass's own shortcuts are ⌃⌥⌘ chords** (F full screen, S stats, W
   disconnect); everything else goes to the PC. macOS keeps ⌘Tab, ⌘Space and
   Mission Control (forwarding those needs a permission the viewer shouldn't
   ask for). Keys are taken with a local event monitor, which also sees ⌘
   combinations; keys released while ⌘ is held (macOS drops their keyUp) are
   released when ⌘ goes up.
5. **Absolute pointer, normalised to the picture** (0..65535, bars excluded),
   mapped by the host onto the captured monitor in virtual-desktop
   coordinates. Button events carry their position so a click lands where it
   was made. LATER: relative mode for games.
6. **Injection with `SendInput`**, keys by scan code. Windows doesn't
   auto-repeat injected keys, so the viewer forwards macOS key repeats.
   Elevated windows need an elevated host; the secure desktop stays out of
   reach (SPEC §10).
7. **Local pointer:** the host sends the pointer shape over TCP only when the
   cursor handle changes (rendered on black and white to recover alpha for any
   cursor type, capped at 96 px) plus shown/hidden; the Mac shows it as its own
   `NSCursor`, scaled like the picture. Pointer movement therefore has no
   network latency at all. WGC capture already excludes the cursor.
8. **Input → screen metric:** frames carry `input_seq` (last input datagram
   applied at capture); the viewer times from sending that datagram to
   presenting the first frame that carries it. That's a lower bound on
   "click to visible reaction" (the app may take longer to redraw) but
   measures the whole pipeline both ways.

## Consequences
- Protocol v2; control frames can now be up to 64 KB (pointer images).
- Injection runs on the host's UDP thread, straight from the socket.
