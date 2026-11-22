//! Glass core: everything both directions (PC -> Mac and Mac -> PC) must agree on.
//!
//! No platform media APIs live here. `app-win` links this as an rlib; `app-mac`
//! links it as a staticlib through the cbindgen header in `include/glass_core.h`.

pub mod annexb;
pub mod clipboard;
pub mod clock;
pub mod ffi;
pub mod identity;
pub mod input;
pub mod keymap;
pub mod net;
pub mod packet;
pub mod pairing;
pub mod protocol;
pub mod recording;
pub mod replay;
pub mod seal;
pub mod stats;
mod tls;

/// Crate version, e.g. "0.0.1".
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// LATER(M4): Reed-Solomon FEC over packet groups.
// LATER(M5): adaptive jitter buffer.
// LATER(M7): content-aware rate-control decisions.
