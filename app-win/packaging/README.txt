Glass for Windows (test build __VERSION__)
=========================================

Needs an NVIDIA GPU (NVENC). Unzip anywhere; keep the DLLs next to glass.exe.
Windows SmartScreen may warn because the build isn't signed: click
"More info" > "Run anyway".

Live stream to the Mac:
  1. Double-click host.cmd. It prints this PC's name and address, e.g.
     192.168.1.20. If Windows Firewall asks, allow glass.exe on Private
     networks. (If your network is marked Public, allow Public too, or switch
     it to Private in Settings > Network & internet.)
  2. On the Mac, open Glass.app, pick this PC from the list (or type the
     address) and click Connect.
     First time: the Mac asks to pair, and this window shows a 6-digit PIN.
     Type it on the Mac. Only paired devices can connect, and everything
     between them is encrypted. Pair only devices you own.
  3. When the Mac disconnects, this window prints the session's numbers and
     saves them to results-host.txt. Paste them back to Claude.
  For 4:4:4 instead of 4:2:0:  host.cmd --chroma 444
  The Mac's mouse and keyboard control this PC while connected. host.cmd
  asks Windows for administrator rights when it starts: answer Yes so the Mac
  can also control admin windows (Task Manager, installers). If you answer
  No it still works, but Windows blocks all of the Mac's input while an admin
  window is in front. UAC prompts and the lock screen need the PC itself.

M1 test (recordings): record-clips.cmd.

Paired devices:
  glass devices               this PC's id and the devices paired with it
  glass forget NAME           unpair a device (it must pair again to connect)
  host.cmd --ask              ask here before each connection
  Identity and pairings live in %APPDATA%\Glass (the key is encrypted with
  your Windows account).

Commands:
  glass host [--monitor N] [--chroma 420|444] [--mbps M] [--port P] [--log FILE] [--ask]
  glass probe HOST [--port P] [--pin PIN] [--seconds S]  headless viewer (network check)
  glass serve FILE.glsr [--port P] [--pin PIN]            stream a recording as if live
  glass list
  glass record [--monitor N] [--seconds S] [--chroma 420|444] [--mbps M] out.glsr
  glass dump in.glsr out.h265

Ports: TCP and UDP 47800 (change with --port; on the Mac, type address:port).

FFmpeg (LGPL) DLLs are included; see FFmpeg-LICENSE.txt.
