//! Keeps the device private key encrypted at rest with DPAPI (tied to the
//! Windows user account; no prompt). Used for `%APPDATA%\Glass\identity.key`.

use glass_core::identity::KeyProtector;
use std::io;
use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

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
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &blob(data),
            w!("Glass device key"),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
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
