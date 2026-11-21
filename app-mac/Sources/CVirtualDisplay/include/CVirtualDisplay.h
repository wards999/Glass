// Virtual displays through macOS's private CGVirtualDisplay classes (the
// approach DeskPad and BetterDisplay use; there is no public API). The
// classes are looked up at run time, so if a macOS update removes them Glass
// still launches and falls back to the Mac's own screen.
#pragma once
#include <stdbool.h>
#include <stdint.h>

/// True if this macOS has the private virtual-display classes.
bool glass_vd_available(void);

/// Creates a virtual display with one mode of `mode_w` x `mode_h` points at
/// `hz`; `hidpi` doubles its pixels (Retina). Returns an opaque handle (NULL
/// on failure) and writes the new display's id. The display exists until
/// `glass_vd_destroy`.
void *glass_vd_create(const char *name, uint32_t mode_w, uint32_t mode_h, double hz, bool hidpi,
                      uint32_t *display_id);

/// Removes the virtual display.
void glass_vd_destroy(void *handle);
