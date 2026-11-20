# 0008 — No Apple Developer ID: stable self-signed signing

**Status:** accepted (applies from M3b) · **Date:** 2026-11-20

## Context
The user won't get an Apple Developer ID. Test builds are ad-hoc signed today.
macOS keys privacy approvals (Screen Recording, Accessibility, Local Network)
to the app's code-signing identity. An ad-hoc signature changes with every
build, so a Mac host would lose its permissions after each update.

## Decision
- Create one self-signed code-signing certificate. CI imports it from an
  encrypted Actions secret into a temporary keychain and signs `Glass.app`
  with it. The bundle ID stays `local.glass.Glass`. Approvals then survive
  updates because the identity stays the same.
- Accept Gatekeeper's first-launch step per downloaded build (right-click →
  Open), and document it.
- Don't use APIs that require notarization or a Developer ID (e.g.
  `SMAppService` privileged daemons). Anything that needs root is a one-time
  admin install step.
- Private macOS APIs are acceptable (no App Store), e.g. `CGVirtualDisplay`.

## Consequences
- Setting this up needs a new repository secret (the certificate and its
  password). It's created at the start of M3b, with the user's OK.
- If the certificate is ever replaced, the Mac asks for the permissions again
  once.
- Windows stays unsigned: SmartScreen "Run anyway" and the firewall prompt per build.
