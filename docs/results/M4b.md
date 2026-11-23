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

## To do
- Steps 2–4 (ADR 0025), then real numbers: audio latency, A/V offset,
  concealed and gaps on Wi-Fi, with a file copy running.
