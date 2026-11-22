Glass for Windows (test build __VERSION__)
=========================================

Hosting (host.cmd) needs an NVIDIA GPU (NVENC); viewing (connect.cmd) works on
any GPU that can decode HEVC. Unzip anywhere; keep the DLLs next to Glass.exe and glass-cli.exe.
Windows SmartScreen may warn because the build isn't signed: click
"More info" > "Run anyway".

The Glass window: double-click Glass.exe.
  - Machines with Glass on your network are listed (Macs and PCs, paired or
    not). Double-click one, or type an address and press Connect.
  - The first time, the other machine shows a 6-digit PIN: type it in the box.
  - Share this PC lets another machine view this one (the window shows the
    PIN when it pairs). Closing the window while sharing keeps Glass in the
    tray (bottom right); right-click the tray icon to stop or quit.
  - Paired devices... lists and forgets pairings.
  - Restart as administrator: lets the viewer control admin windows too.
  - Results of each session are saved next to Glass.exe and shown at the end.
  host.cmd and connect.cmd below still work the same way.

Live stream to the Mac:
  1. Double-click host.cmd. It prints this PC's name and address, e.g.
     192.168.1.20. If Windows Firewall asks, allow Glass on Private
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

View and control the Mac from this PC (connect.cmd):
  1. On the Mac, open Glass and click "Share This Mac". It shows the Mac's
     address (and asks for Screen Recording and Accessibility the first time).
  2. Double-click connect.cmd and type that address. The first time, the Mac
     shows a 6-digit PIN: type it here. The Mac opens full screen.
     Ctrl+Alt+Shift+F   window / full screen
     Ctrl+Alt+Shift+R   view: the Mac's screen, or fit to this PC (like now /
                        more space / larger); remembered for that Mac
  To start in a particular view:  connect.cmd --view mac   (or fit, space, larger)
     Ctrl+Alt+Shift+C   full colour (4:4:4) on/off. Off by default (4:2:0 is faster);
                        to start with it on: connect.cmd --colour 444
     Ctrl+Alt+Shift+Q   disconnect (results print here and go to results-viewer.txt)
  Every other key goes to the Mac. Ctrl acts as Cmd (Ctrl+C copies), Win as
  Ctrl, Alt as Option. For a PC host instead: connect.cmd --keys labelled.

M1 test (recordings): record-clips.cmd.

Paired devices:
  glass-cli devices               this PC's id and the devices paired with it
  glass-cli forget NAME           unpair a device (it must pair again to connect)
  host.cmd --ask              ask here before each connection
  Identity and pairings live in %APPDATA%\Glass (the key is encrypted with
  your Windows account).

Commands (glass-cli.exe, from a Command Prompt in this folder):
  glass-cli host [--monitor N] [--chroma 420|444] [--mbps M] [--port P] [--log FILE] [--ask]
  glass-cli connect [HOST[:PORT]] [--pin PIN] [--windowed] [--keys pc|labelled] [--log FILE]
  glass-cli probe HOST [--port P] [--pin PIN] [--seconds S]  headless viewer (network check)
  glass-cli serve FILE.glsr [--port P] [--pin PIN]            stream a recording as if live
  glass-cli list
  glass-cli record [--monitor N] [--seconds S] [--chroma 420|444] [--mbps M] out.glsr
  glass-cli dump in.glsr out.h265

Ports: TCP and UDP 47800 (change with --port; on the Mac, type address:port).

FFmpeg (LGPL) DLLs are included; see FFmpeg-LICENSE.txt.

Unattended access (the Glass service, M3f):
  To use this PC from far away, including its lock screen, sign-in and UAC
  prompts: Glass > gear > Unattended access > Install service (Windows asks
  for administrator rights), or as administrator:  glass-cli service install
  It copies this build to C:\Program Files\Glass, keeps this PC's pairings,
  allows it through Windows Firewall on private networks and starts it.
  Installing again from a newer build updates it.
  The Allow connections switch turns its sharing off (it stays off after restarts)
  and on again, no admin rights needed. As administrator you can also stop it
  completely:  glass-cli service off | on
  Remove it:  Settings > Remove, or  glass-cli service uninstall
  Log: C:\ProgramData\Glass\service.log

Sharper pictures from a PC (resolution matching):
  While a viewer is connected, this PC switches its screen to the mode that
  best fits the viewer's screen, and switches back afterwards. It can only use
  modes the monitor or graphics driver offers: on a small monitor, NVIDIA DSR
  (NVIDIA Control Panel > Manage 3D settings > DSR - Factors) or an HDMI dummy
  plug adds bigger ones. To keep the resolution:  glass-cli host --keep-resolution
  The service keeps each session's results in C:\ProgramData\Glassgent-results.txt
