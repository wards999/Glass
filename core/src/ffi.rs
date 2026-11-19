//! C ABI for the Swift app. Header: `core/include/glass_core.h` (cbindgen).

use std::ffi::c_char;

static VERSION_C: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// NUL-terminated core version string, e.g. "0.0.1". Static; do not free.
#[no_mangle]
pub extern "C" fn glass_core_version() -> *const c_char {
    VERSION_C.as_ptr().cast()
}

/// Wire protocol version (see docs/protocol.md).
#[no_mangle]
pub extern "C" fn glass_protocol_version() -> u8 {
    crate::protocol::PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn version_over_ffi_matches_crate() {
        let s = unsafe { CStr::from_ptr(glass_core_version()) };
        assert_eq!(s.to_str().unwrap(), crate::VERSION);
        assert_eq!(glass_protocol_version(), crate::protocol::PROTOCOL_VERSION);
    }
}
