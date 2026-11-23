# 0008 — No trusted code signing on either platform

**Status:** accepted (Mac from M3b; widened to Windows 2026-11-23) · **Date:** 2026-11-20

## Context
Glass can't be signed with a trusted certificate on either platform. There is
no Apple Developer ID (so no notarization) and no Windows code-signing
certificate (Authenticode / EV). Every design has to work without one.

On the Mac, privacy approvals (Screen Recording, Accessibility, Local
Network) are tied to the app's code-signing identity. An ad-hoc signature
changes with every build, so a Mac host would lose its permissions after
each update.

## Decision
**Mac**
- Use one self-signed code-signing certificate. CI imports it from an
  encrypted Actions secret into a temporary keychain and signs `Glass.app`
  with it. The bundle ID stays `local.glass.Glass`. Approvals survive updates
  because the identity stays the same.
- Accept Gatekeeper's first-launch step for each downloaded build
  (right-click → Open, or Privacy & Security → Open Anyway), and document it.
- Don't use anything that needs notarization or a Developer ID: no
  `SMAppService` privileged daemons, system or network extensions, DriverKit,
  or Developer-ID-only entitlements. Anything that needs root is a one-time
  admin install step.
- Private macOS APIs are acceptable (no App Store), e.g. `CGVirtualDisplay`.

**Windows**
- `Glass.exe` and `glass-cli.exe` are unsigned. Expect SmartScreen's "More
  info → Run anyway" on each downloaded build. The firewall prompt can come
  back after an update, and UAC shows "Unknown publisher" when installing the
  service.
- No kernel drivers of our own: Windows only loads signed drivers. Where a
  driver is needed, reuse an existing vendor-signed one, e.g. an open-source
  IddCx virtual display driver for M7, or ViGEmBus for M11.
- Antivirus may flag unsigned builds that inject input or capture the
  screen. If it does, the user allows Glass once. We never try to evade
  detection.

**Both**
- Check every new feature against this ADR before choosing an approach. If
  something only works with a trusted signature, find another way, or say
  that it can't be done.

## Consequences
- The Mac needs a repository secret for the certificate and its password.
  It was created at the start of M3b, with the user's OK. If the certificate
  is ever replaced, the Mac asks for its permissions again once.
- Each test build takes one or two clicks to run on each machine. README and
  `docs/testing.md` explain them.
