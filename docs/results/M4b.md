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

## To do
- Steps 2–4 (ADR 0025), then real numbers: audio latency, A/V offset,
  concealed and gaps on Wi-Fi, with a file copy running.
