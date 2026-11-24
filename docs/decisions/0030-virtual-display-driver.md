# 0030 — The Windows virtual display: the Virtual Display Driver, installed from Settings

**Status:** accepted · **Date:** 2026-11-24 · M5 (Round 1, ADR 0028)

## Context
A Windows host can only share its real monitor's modes, so a viewer of
another shape gets black bars or scaling (QUIETPC22's 2560×1440 on a
2940×1912 MacBook). Windows has no API for an app to add a screen: it takes
an IddCx driver, which must be signed, and Glass signs nothing (ADR 0008).
StarDesk ships its own Microsoft-attested one.

## Decision
- **The open-source Virtual Display Driver** (VirtualDrivers, MIT), release
  25.7.23, driver files only (`MttVDD.inf`, `MttVDD.dll`, `mttvdd.cat`):
  signed by SignPath Foundation through GlobalSign (valid on QUIETPC22).
  Fetched by `scripts/fetch-vdd.ps1`, pinned by SHA-256, shipped in the
  Windows zip's `vdd` folder with its licence. Not its control app.
- **Settings → Virtual display → Install** runs `glass-cli vdisplay
  install` as administrator (one UAC prompt; Windows may also ask to trust
  the publisher): a settings folder the PC's users can write
  (`C:\ProgramData\Glass\vdd`, pointed to by the driver's `VDDPATH`), the
  root device `Root\MttVDD` created with SetupAPI, the driver installed on
  it. **Remove** takes out the device, the driver package and the key.
- **Per session, without admin** (next step): Glass writes the viewer's
  exact size and refresh into the driver's settings and reloads it through
  the driver's own pipe (`\.\pipe\MTTVirtualDisplayPipe`, open to users),
  shares that screen, and takes it out of the desktop afterwards.

## Consequences
- Until the session step lands, an installed driver shows as a second
  screen in Windows' Display settings.
- Parsec's driver stays the fallback if this one's signature isn't
  accepted somewhere.

## Addendum (m5-test19): used during sessions
- The user chose: **the PC's own monitor off while viewed**; the whole
  desktop moves to the virtual display, and comes back afterwards.
- `Hello` now carries the viewer's display scaling (protocol v15). The
  virtual display is the size the viewer's screen looks like (a 200 %
  MacBook at 2940×1912 → 1470×956), at its refresh: the same text size as
  on the Mac. Full resolution with Windows at 200 % (sharper, if Windows
  scales the apps well) is to be compared next, from the bar.
- Per session (`vscreen.rs`): the driver's settings get exactly that mode
  and it reloads (its pipe); the virtual display becomes the only active
  path (`SetDisplayConfig`, not saved), then takes the mode. At the end
  Windows' saved layout returns (`SDC_USE_DATABASE_CURRENT`). The saved
  layout never includes the virtual display (`detach_saved`, after install
  and before each session), so a crash or restart brings the PC's monitor
  back. If anything fails, the session uses the PC's own screen and the
  results say why.
