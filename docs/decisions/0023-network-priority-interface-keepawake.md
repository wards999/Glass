# 0023 — The network itself: packet priority, the right adapter, radios kept awake

**Status:** accepted · **Date:** 2026-11-23 · M4 step 4 (ADR 0020)

## Decision
- **Packet marking** (`core/src/qos.rs`):
  - Video, from the host, is marked as video: DSCP AF41, Wi-Fi's AC_VI
    queue. Input, NACKs and keep-awakes, from the viewer, are marked as
    voice: DSCP EF, AC_VO.
  - On the Mac: `SO_NET_SERVICE_TYPE` (`RV` / `VO`), which the Mac's own
    Wi-Fi driver queues by, plus `IP_TOS`.
  - On Windows: qWAVE's `QOSAddSocketToFlow` with the `AudioVideo` /
    `Voice` traffic types, towards the peer's address. Windows ignores
    `IP_TOS` from programs, but qWAVE needs no admin rights for these
    types. The host redoes the marking when the viewer's address changes.
  - The results say whether the OS accepted it. Whether the router honours
    DSCP can only be seen in a packet capture. Where it isn't honoured,
    marking changes nothing.
- **The right adapter.**
  - A Windows host now advertises the address of an up Ethernet or Wi-Fi
    adapter with an IPv4 default gateway, lowest route metric first.
    Adapters are chosen by kind, not by name. VPN tunnels (Tailscale,
    WireGuard) are virtual adapter types. WSL's and Hyper-V's internal
    switches have no gateway. Link-local addresses are skipped. Checked on
    QUIETPC22: 192.168.4.43 (Wi-Fi), with the Wi-Fi Direct and Bluetooth
    adapters left out.
  - The Mac already put `en*` (Wi-Fi, Ethernet) first and skipped
    link-local addresses.
- **Radios kept awake.**
  - During a session each side sends something at least every 10 ms. The
    viewer sends a punch, which its input and NACKs already do while busy.
    The host sends a sealed `"GLKA"` keep-awake datagram (protocol v12),
    which video already does while the screen moves.
  - This stops Wi-Fi power save from adding its wake-up delay to the first
    frame after a still moment.
  - The threads use `try_lock`, so a frame or input never waits for them.
  - The cost is up to 100 small datagrams a second each way, about
    5 KB/s.
- **MTU: deferred.** Datagrams stay at 1400 bytes without the DF bit, as
  before. A probe (DF set, a full-size test datagram, falling back to 1200)
  changes the packet sizes throughout the packetiser. It's only worth doing
  if a VPN or mesh setup shows the loss pattern (big packets lost, small
  ones not). LATER(M6): with the VPN warning.

## Consequences
- Protocol v12: an older viewer would count the keep-awakes as bad
  packets. Both ends update together, as always.
- The results gain a line: "network (M4): … marked for Wi-Fi priority:
  yes | keep-awakes sent N".
