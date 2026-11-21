//! Advertises the host on the local network as `_glass._tcp` (DNS-SD over
//! mDNS) through Windows' own responder (`DnsServiceRegister`, Windows 10
//! 1809+), so viewers can list it by name. Uses the system service, so no
//! extra firewall rule or port 5353 listener of our own.
//!
//! TXT: `v` protocol version, `fp` first 8 bytes of the certificate
//! fingerprint (hex, for "is this the PC I paired with?"), `ip` and `port`
//! where the viewer should connect, `os` (`windows` / `mac`).

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::NetworkManagement::Dns::{
    DnsServiceConstructInstance, DnsServiceRegister, DNS_SERVICE_REGISTER_REQUEST,
};

const DNS_QUERY_REQUEST_VERSION1: u32 = 1;
const DNS_REQUEST_PENDING: u32 = 9506;

/// Last registration status (0 = registered, u32::MAX = pending).
static STATUS: AtomicU32 = AtomicU32::new(u32::MAX);

unsafe extern "system" fn done(
    status: u32,
    _ctx: *const core::ffi::c_void,
    _inst: *const windows::Win32::NetworkManagement::Dns::DNS_SERVICE_INSTANCE,
) {
    STATUS.store(status, Ordering::Relaxed);
}

/// Instance labels can't contain dots.
fn label(name: &str) -> String {
    name.replace('.', "-")
}

/// Starts advertising; lasts until the process exits. Errors are returned
/// as text for the console (discovery is optional: typing the address works).
pub fn advertise(
    name: &str,
    port: u16,
    ip: Option<Ipv4Addr>,
    fp_short: &str,
) -> Result<(), String> {
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "glass-pc".into());
    let service = HSTRING::from(format!("{}._glass._tcp.local", label(name)));
    let hostname = HSTRING::from(format!("{}.local", label(&host)));
    let mut kv: Vec<(HSTRING, HSTRING)> = vec![
        (
            "v".into(),
            glass_core::protocol::PROTOCOL_VERSION.to_string().into(),
        ),
        ("fp".into(), fp_short.replace(' ', "").into()),
        ("port".into(), port.to_string().into()),
        ("os".into(), "windows".into()),
    ];
    if let Some(ip) = ip {
        kv.push(("ip".into(), ip.to_string().into()));
    }
    let keys: Vec<PCWSTR> = kv.iter().map(|(k, _)| PCWSTR(k.as_ptr())).collect();
    let values: Vec<PCWSTR> = kv.iter().map(|(_, v)| PCWSTR(v.as_ptr())).collect();
    let ip4 = ip.map(|a| u32::from_ne_bytes(a.octets()));
    unsafe {
        let inst = DnsServiceConstructInstance(
            &service,
            &hostname,
            ip4.as_ref().map(|v| v as *const u32),
            None,
            port,
            0,
            0,
            kv.len() as u32,
            keys.as_ptr(),
            values.as_ptr(),
        );
        if inst.is_null() {
            return Err("DnsServiceConstructInstance failed".into());
        }
        // Must stay valid while registered (and for DnsServiceDeRegister).
        let req = Box::leak(Box::new(DNS_SERVICE_REGISTER_REQUEST {
            Version: DNS_QUERY_REQUEST_VERSION1,
            InterfaceIndex: 0,
            pServiceInstance: inst,
            pRegisterCompletionCallback: Some(done),
            ..Default::default()
        }));
        match DnsServiceRegister(req, None) {
            DNS_REQUEST_PENDING => {
                *REGISTERED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Req(req));
                Ok(())
            }
            e => Err(format!("DnsServiceRegister error {e}")),
        }
    }
}

struct Req(&'static mut DNS_SERVICE_REGISTER_REQUEST);
unsafe impl Send for Req {}
static REGISTERED: std::sync::Mutex<Option<Req>> = std::sync::Mutex::new(None);

/// Stops advertising (sharing stopped).
pub fn unadvertise() {
    if let Some(Req(req)) = REGISTERED.lock().unwrap_or_else(|e| e.into_inner()).take() {
        unsafe {
            let _ = windows::Win32::NetworkManagement::Dns::DnsServiceDeRegister(req, None);
        }
    }
    STATUS.store(u32::MAX, Ordering::Relaxed);
}

/// `Some(true)` registered, `Some(false)` failed, `None` still pending.
pub fn registered() -> Option<bool> {
    match STATUS.load(Ordering::Relaxed) {
        u32::MAX => None,
        0 => Some(true),
        _ => Some(false),
    }
}
