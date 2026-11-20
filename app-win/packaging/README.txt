Glass for Windows (test build __VERSION__)
=========================================

Needs an NVIDIA GPU (NVENC) for recording. Unzip anywhere; keep the DLLs next
to glass.exe. Windows SmartScreen may warn because the build isn't signed:
click "More info" > "Run anyway".

M1 test:
  Double-click record-clips.cmd. It records two 10-second clips of your main
  display (HEVC 4:2:0, then 4:4:4) into this folder. While it records, use the
  desktop normally: scroll some text, drag a window, minimise and restore.
  The results are saved to results-win.txt; paste them back to Claude.
  Copy desktop-420.glsr and desktop-444.glsr to the Mac and open them with Glass.app.

Commands:
  glass list
  glass record [--monitor N] [--seconds S] [--chroma 420|444] [--mbps M] out.glsr
  glass dump in.glsr out.h265

FFmpeg (LGPL) DLLs are included; see FFmpeg-LICENSE.txt.
