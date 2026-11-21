//! Keeps the device private key encrypted at rest with DPAPI (tied to the
//! Windows user account; no prompt). Used for `%APPDATA%\Glass\identity.key`.
//! The service's agent (M3f) runs as SYSTEM and keeps its key with
//! machine-scope DPAPI in a folder only SYSTEM and administrators can read
//! (`set_machine_scope`).

use glass_core::identity::KeyProtector;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN,
    CRYPT_INTEGER_BLOB,
};

static MACHINE: AtomicBool = AtomicBool::new(false);

/// New keys are protected for the machine, not the user (the service's
/// agent, which runs as SYSTEM).
pub fn set_machine_scope() {
    MACHINE.store(true, Ordering::Relaxed);
}

/// Re-encrypts a user-scope key blob for the machine (the installer, run
/// elevated as the same user, moves this PC's identity to the service).
pub fn user_to_machine(stored: &[u8]) -> io::Result<Vec<u8>> {
    let plain = unprotect(stored)?;
    protect_with(&plain, true)
}

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    }
}

/// Copies DPAPI's output and frees it.
unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
    let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
    v
}

fn protect(data: &[u8]) -> io::Result<Vec<u8>> {
    protect_with(data, MACHINE.load(Ordering::Relaxed))
}

fn protect_with(data: &[u8], machine: bool) -> io::Result<Vec<u8>> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    let flags = if machine {
        CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE
    } else {
        CRYPTPROTECT_UI_FORBIDDEN
    };
    unsafe {
        CryptProtectData(
            &blob(data),
            w!("Glass device key"),
            None,
            None,
            None,
            flags,
            &mut out,
        )
        .map_err(io::Error::other)?;
        Ok(take(out))
    }
}

fn unprotect(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &blob(data),
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| {
            io::Error::other(format!(
                "can't decrypt the device key (made by another Windows user?): {e}"
            ))
        })?;
        Ok(take(out))
    }
}

pub static PROTECTOR: KeyProtector = KeyProtector { protect, unprotect };

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let secret = b"glass device key test".to_vec();
        let p = super::protect(&secret).unwrap();
        assert_ne!(p, secret);
        assert_eq!(super::unprotect(&p).unwrap(), secret);
        assert!(super::unprotect(b"not a dpapi blob").is_err());
    }
}
