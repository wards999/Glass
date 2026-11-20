Glass for Windows (test build __VERSION__)
=========================================

Needs an NVIDIA GPU (NVENC). Unzip anywhere; keep the DLLs next to glass.exe.
Windows SmartScreen may warn because the build isn't signed: click
"More info" > "Run anyway".

M2 test (live stream to the Mac):
  1. Double-click host.cmd. It prints this PC's address, e.g. 192.168.1.20.
     If Windows Firewall asks, allow glass.exe on Private networks.
     (If your network is marked Public, allow Public too, or switch it to
     Private in Settings > Network & internet.)
  2. On the Mac, open Glass.app, type that address and click Connect.
  3. When the Mac disconnects, this window prints the session's numbers and
     saves them to results-host.txt. Paste them back to Claude.
  For 4:4:4 instead of 4:2:0:  host.cmd --chroma 444
  The Mac's mouse and keyboard control this PC while connected. To control
  programs running as administrator, right-click host.cmd > Run as
  administrator (Windows blocks input into them otherwise).

M1 test (recordings): record-clips.cmd.

Commands:
  glass host [--monitor N] [--chroma 420|444] [--mbps M] [--port P] [--log FILE]
  glass probe HOST [--port P] [--seconds S]     headless viewer (network check)
  glass serve FILE.glsr [--port P]               stream a recording as if live
  glass list
  glass record [--monitor N] [--seconds S] [--chroma 420|444] [--mbps M] out.glsr
  glass dump in.glsr out.h265

Ports: TCP and UDP 47800 (change with --port; on the Mac, type address:port).

FFmpeg (LGPL) DLLs are included; see FFmpeg-LICENSE.txt.
