# 0013 — Game controllers split out of M5b

**Status:** accepted · **Date:** 2026-11-21

## Context
M5b (ADR 0005) bundled everything gaming needs: audio, game controllers,
relative mouse, game-mode overlay and even cadence. Controllers are the most
separable and the riskiest part: a third-party driver on Windows (ViGEmBus, no
longer actively developed), anti-cheat that rejects virtual pads, and no
settled way to fake a pad on a Mac host.

## Decision
The user asked to split M5b and make controllers a future milestone.
- **M5b – Gaming: audio and feel** keeps audio with A/V sync, relative mouse
  (mouse-look), the game-mode overlay and even cadence, measured with a real
  game played with mouse and keyboard.
- **M11 – Game controllers (future)** takes controllers in both directions,
  ViGEmBus on a Windows host, rumble. The "audio and controller for the
  Mac-host direction" item leaves M8: audio stays there, controllers go to M11.
- The §0 gaming bar no longer requires a controller until M11.

## Consequences
M5b gets smaller and has no driver install. Controller-first games can't be
judged until M11. SPEC §5c keeps the controller design, marked M11.
