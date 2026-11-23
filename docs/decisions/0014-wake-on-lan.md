# 0014 — Wake-on-LAN

**Status:** accepted · **Date:** 2026-11-21

## Context
Compared with Windows Remote Desktop, Glass was missing multiple monitors,
microphone, remote access from outside the home network, device redirection
(printers, drives, USB, webcam, smart cards), separate login sessions and
Wake-on-LAN. The user chose to add only Wake-on-LAN.

## Decision
- New **M3e – Wake-on-LAN** straight after M3d, since it extends the device
  list. The host sends its adapters' MAC addresses when a viewer connects
  (protocol v11 or later: v8-v10 went to the clipboard and FEC). The viewer keeps them with the paired device. Sleeping paired
  devices get a **Wake** button that sends the magic packet, waits for the
  device to reappear, then connects.
- Glass never changes power or adapter settings itself (the standing rule). It
  detects what is off and says where to turn it on.
- The rest stays out of scope for now: multiple monitors, microphone,
  internet access, redirection and extra sessions.

## Consequences
- Wake is reliable over Ethernet only. Most PCs can't wake over Wi-Fi, and a
  Mac on Wi-Fi needs a Bonjour Sleep Proxy (Apple TV/HomePod). The app says
  so instead of failing silently.
- The protocol goes to v8, so both ends must update together, as usual.
