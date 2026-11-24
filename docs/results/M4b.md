# M4b — Audio: results

Status: **in progress** (steps 1–2 of 4 done; ADR 0025).

## Step 1: core (codec, redundancy, NACK, playout)
- libopus 48 kHz stereo, 10 ms, restricted low-delay, 192 kbps.
- Unit tests: clean, 2 lost in a row, 5 % random, a 30 ms outage, a long
  outage, ±0.3 % clock drift, and no clicks. All pass.
- losssim (100 s per model, 4 ms RTT): 5 % random loss gives 0 concealed
  and 0 gaps. Heavy bursts (3.9 %) give 1 concealed frame and 4 short gaps,
  and the buffer grows to 30 ms.

## Step 2: PC → Mac (m4b-test1)
- WASAPI loopback (48 kHz float, converted by Windows) on the in-app
  host, and AVAudioEngine playback on the Mac viewer.
- The host's speakers are muted for the session. On QUIETPC22, loopback
  still hears through the mute at full level.
- Real numbers to come.

### m4b-test1 (the Mac viewing QUIETPC22): sound half a second behind
The user: "it was half a second behind, which meant movies and games were
unusable". The cause:
- The Mac's output started after the sound had begun to arrive: up to
  150 ms for the viewer to notice, plus the audio engine's start-up.
- Frames waiting, not yet decoded, were never counted. Only the small
  decoded buffer was held at its target.
- So the start-up backlog played in order, at real time, for the whole
  session.
- The same blind spot would also have let a host clock running fast build
  up delay over a long session.

Fixed in m4b-test2:
- All the waiting sound (buffer plus undecoded frames) is held at the
  target, 20–30 ms.
- Before the device reads, only the newest frames are kept.
- After a stall leaves more than 40 ms over the target, playback skips to
  the newest sound with a 10 ms fade-in.
- The drift control steers by the total.
- New tests: the output starting 500 ms late, a 300 ms stall, and drift
  ±0.3 %, each with the delay held under 50 ms.
- The results add "A/V (M4b): sound X ms behind/ahead of the picture",
  including the output device's own latency. The Mac's overlay shows the
  sound's delay live.
- Holding the total at 20 ms took away the hidden margin that had covered
  losses, so 3 redundant copies (was 2), concealment instead of a silent
  gap, and a 50 ms cap on the buffer.
- losssim now (100 s per model, 4 ms round trip):

  | loss | concealed | gaps | buffer |
  |---|---|---|---|
  | random 0–2 % | 0 | 0 | 20 ms |
  | random 5 % | 3 (0.03 %) | 0 | 20 ms |
  | random 10 % | 3 (0.03 %) | 0 | 20 ms |
  | bursts 1.1 % | 10 (0.10 %) | 0 | 45 ms |
  | bursts 3.9 % | 20 (0.20 %) | 7 | 50 ms |

### m4b-test2 (the Mac viewing QUIETPC22, a Steam game, 299 s)
- **Sound:** 30 ms from capture to the Mac's speakers (the output adds
  1 ms). The half-second delay is fixed.
- **Picture:** 53 ms (capture → screen p50), so the sound came **23 ms
  ahead**.
  - The PC sent up to 165 fps (75 arrived) to a 60 Hz Mac, which presented
    37.5 fps, each frame shown for two refreshes, with 32 ms in present.
  - Its decode queue overflowed 25 times: 39 keyframe requests, and
    5 hitches averaging 138 ms.
  - That's M5's "frames at the viewer's refresh", needed now.
- **Sound glitches:** 45 concealed, 11 gaps, with almost nothing lost (one
  frame rebuilt, no resends). The never-a-gap rule concealed frames that
  were only a little late, then dropped them when they came.

Fixed in m4b-test3:
- A frame is concealed only when it's lost (a newer one is here) or
  overdue (nothing for 15 ms). A test with frames 8–14 ms late gives no
  concealment.
- **A/V sync follows the picture.** Once a second, the Mac viewer sets how
  much sound to hold so it plays about 5 ms after its picture shows
  (20–80 ms). A test holding 55 ms gives 50–65 ms latency, with no gaps.
- losssim: no audio gaps in any loss model; concealed 0.03 % at 5 %.

## To do
- Steps 2–4 (ADR 0025), then real numbers: audio latency, A/V offset,
  concealed and gaps on Wi-Fi, with a file copy running.

## Step 3: Mac → PC (m5-test17)
- **Mac host:** ScreenCaptureKit captures everything the Mac plays (not
  Glass's own sound) on its own stream, 48 kHz stereo, cut into 10 ms
  frames and sent through core (`glass_session_audio_out`,
  `glass_audio_out_send`): the same Opus path as a PC host. The Mac's
  default output is muted for the session and restored after.
- **Windows viewer:** plays the ring core fills (`play.rs`): WASAPI shared
  mode, event-driven, 10 ms period. Once a second it holds the sound so it
  plays about 5 ms after the picture, as the Mac viewer does.
- Also PC → PC: a Windows viewer now plays a PC host's sound.
- To see: whether ScreenCaptureKit still hears the Mac's sound with its
  output muted, and whether it sends while nothing plays (the "silent"
  count).
