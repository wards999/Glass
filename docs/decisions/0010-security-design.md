# 0010 — Security design (M6)

**Status:** accepted · **Date:** 2026-11-20

## Context
A Glass host lets a viewer see the screen and control the mouse and keyboard.
Only devices the owner has paired may connect, and nobody else on the
network may read, alter, replay or inject anything. There is no server, no
account and no Apple Developer ID (ADR 0008), so trust has to come from the
two devices themselves.

## Decision
- **Identity:** each device has a long-lived ECDSA P-256 key and a
  self-signed certificate (`rcgen`), made on first run. The device *is* the
  SHA-256 fingerprint of its certificate.
- **Pairing:** a 6-digit PIN shown on the host, typed on the viewer, run as
  SPAKE2 (Ed25519) inside a TLS 1.3 connection. The SPAKE2 identities are both
  fingerprints and the confirmation MACs include a TLS exporter value, so
  a man in the middle fails unless it guesses the PIN. Each attempt gives it
  one guess, counted before the host answers. Limits: 3 per PIN, 10 per host
  run, then pairing is off until the host restarts. That is at most 10 guesses
  in 10^6 per run.
  Rejected: comparing fingerprints by eye (people skip it) and a QR code (the
  PC side is a console for now).
- **Every connection:** mutual TLS 1.3 only (`rustls` with the `ring`
  provider). The verifiers check the CertificateVerify signature. Right after
  the handshake each side checks the peer's fingerprint against its paired
  list. No CA, no hostnames, no resumption.
  - The viewer refuses an unknown host before sending anything about itself.
  - The host gives an unknown viewer no session, and no input is ever applied.
- **Media:** every UDP datagram is sealed with AES-128-GCM (`ring`).
  - Per-session, per-direction keys come from the TLS exporter.
  - A 64-bit counter is the nonce source: it is never reused, and an HKDF
    ratchet rolls the key every 2^23 packets, far inside the AES-GCM limits.
  - A 1024-packet replay window drops duplicates and old packets.
  - It adds 24 bytes, and the video chunk shrinks to 1352 so sealed
    datagrams stay ≤ 1400.
  - Measured cost: about 0.3 µs to seal and open one 1400-byte datagram on
    the test PC.
  - Rejected: DTLS or QUIC. They bring more machinery than one AEAD with keys
    from the TLS session we already have, and they make it harder to keep
    control of pacing and FEC (M4, M5).
- **Permissions:** two roles per paired device, *host* (I view it) and
  *viewer* (it may view and control me). Pairing grants one direction only.
  The host can optionally ask before each connection (`--ask`). `glass forget`
  (PC) and "Paired Devices ▸ Forget" (Mac) unpair.
- **Storage:**
  - Windows: the key is DPAPI-encrypted (user account) in `%APPDATA%\Glass`.
  - Mac: a 0600 file in a 0700 folder in `~/Library/Application Support/Glass`.
    The Keychain waits for a stable signing certificate (ADR 0008), because
    every ad-hoc build would trigger a Keychain prompt.
  - `peers.txt` is plain text that people can read. It is re-read on every
    lookup, so forgetting a device takes effect at once.
- **Discovery:** `_glass._tcp` over mDNS, through the Windows system
  responder and the Mac's NWBrowser. It is advisory only: a listing never
  makes a device trusted.

## Consequences
- Protocol v4. Older test builds can't talk to this one.
- Re-installing Glass keeps the identity. Deleting the folder, or a new
  Windows user, means pairing again.
- The host serves one connection at a time, so a pairing attempt waits while
  someone is viewing. More than one viewer at a time is for later (M7).
- Not covered: malware already running on either machine. It could read the
  key file, or simply the screen.
