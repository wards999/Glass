//! Lists Glass hosts on the local network (`_glass._tcp`, DNS-SD over mDNS)
//! through Windows' own resolver (`DnsServiceBrowse` / `DnsServiceResolve`,
//! Windows 10 1809+). Advisory only: trust comes from pairing.

use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::NetworkManagement::Dns::{
    DnsFree, DnsFreeRecordList, DnsServiceBrowse, DnsServiceBrowseCancel, DnsServiceFreeInstance,
    DnsServiceResolve, DNS_RECORDW, DNS_SERVICE_BROWSE_REQUEST, DNS_SERVICE_BROWSE_REQUEST_0,
    DNS_SERVICE_CANCEL, DNS_SERVICE_INSTANCE, DNS_SERVICE_RESOLVE_REQUEST,
};

const DNS_TYPE_PTR: u16 = 12;
const DNS_QUERY_REQUEST_VERSION1: u32 = 1;

/// A Glass host seen on the network.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Its name ("Simon's MacBook Air").
    pub name: String,
    pub address: String,
    pub port: u16,
    /// First 8 bytes of its certificate fingerprint, hex (from TXT).
    pub fp: Option<String>,
    /// "mac" or "windows" (from TXT; older builds don't say).
    pub os: Option<String>,
    pub version: Option<String>,
    seen: Instant,
}

/// The running browse's cancel handle (Windows owns what it points to).
struct Cancel(Box<DNS_SERVICE_CANCEL>);
unsafe impl Send for Cancel {}

struct State {
    found: Vec<Found>,
    cancel: Option<Cancel>,
}

static STATE: Mutex<State> = Mutex::new(State {
    found: Vec::new(),
    cancel: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe fn wstr(p: PWSTR) -> String {
    if p.is_null() {
        String::new()
    } else {
        p.to_string().unwrap_or_default()
    }
}

/// "Simon's MacBook Air._glass._tcp.local" -> "Simon's MacBook Air".
fn instance_label(full: &str) -> String {
    full.split("._glass._tcp")
        .next()
        .unwrap_or(full)
        .to_string()
}

unsafe extern "system" fn resolved(
    status: u32,
    _ctx: *const c_void,
    inst: *const DNS_SERVICE_INSTANCE,
) {
    if status != 0 || inst.is_null() {
        return;
    }
    let i = &*inst;
    let mut f = Found {
        name: instance_label(&wstr(i.pszInstanceName)),
        address: String::new(),
        port: i.wPort,
        fp: None,
        os: None,
        version: None,
        seen: Instant::now(),
    };
    if !i.ip4Address.is_null() {
        f.address = std::net::Ipv4Addr::from(u32::from_be(*i.ip4Address)).to_string();
    }
    for k in 0..i.dwPropertyCount as usize {
        let key = wstr(*i.keys.add(k));
        let val = wstr(*i.values.add(k));
        match key.as_str() {
            "ip" if !val.is_empty() => f.address = val,
            "port" => f.port = val.parse().unwrap_or(f.port),
            "fp" => f.fp = Some(val),
            "os" => f.os = Some(val),
            "v" => f.version = Some(val),
            _ => {}
        }
    }
    DnsServiceFreeInstance(inst);
    if f.address.is_empty() {
        return;
    }
    let mut s = state();
    s.found.retain(|o| o.name != f.name);
    s.found.push(f);
}

unsafe extern "system" fn browsed(status: u32, _ctx: *const c_void, rec: *const DNS_RECORDW) {
    if status != 0 || rec.is_null() {
        return;
    }
    let mut r = rec;
    while !r.is_null() {
        let d = &*r;
        if d.wType == DNS_TYPE_PTR {
            let name = wstr(d.Data.PTR.pNameHost);
            if d.dwTtl == 0 {
                // Goodbye: the host stopped sharing.
                let label = instance_label(&name);
                state().found.retain(|o| o.name != label);
            } else {
                // The request's name must outlive the async resolve.
                let q: &'static HSTRING = Box::leak(Box::new(HSTRING::from(name)));
                let req = DNS_SERVICE_RESOLVE_REQUEST {
                    Version: DNS_QUERY_REQUEST_VERSION1,
                    InterfaceIndex: 0,
                    QueryName: PWSTR(q.as_ptr() as *mut u16),
                    pResolveCompletionCallback: Some(resolved),
                    pQueryContext: std::ptr::null_mut(),
                };
                let cancel = Box::leak(Box::new(DNS_SERVICE_CANCEL::default()));
                let _ = DnsServiceResolve(&req, cancel);
            }
        }
        r = d.pNext;
    }
    DnsFree(Some(rec as *const c_void), DnsFreeRecordList);
}

/// Starts (or restarts) listening for Glass hosts.
pub fn start() -> Result<(), String> {
    stop();
    let q: &'static HSTRING = Box::leak(Box::new(HSTRING::from("_glass._tcp.local")));
    let req = DNS_SERVICE_BROWSE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1,
        InterfaceIndex: 0,
        QueryName: PCWSTR(q.as_ptr()),
        Anonymous: DNS_SERVICE_BROWSE_REQUEST_0 {
            pBrowseCallback: Some(browsed),
        },
        pQueryContext: std::ptr::null_mut(),
    };
    let mut cancel = Box::new(DNS_SERVICE_CANCEL::default());
    let r = unsafe { DnsServiceBrowse(&req, &mut *cancel) };
    // DNS_REQUEST_PENDING (9506) means it's running.
    if r != 9506 {
        return Err(format!("DnsServiceBrowse error {r}"));
    }
    state().cancel = Some(Cancel(cancel));
    Ok(())
}

pub fn stop() {
    if let Some(c) = state().cancel.take() {
        unsafe {
            let _ = DnsServiceBrowseCancel(&*c.0);
        }
    }
}

/// Hosts seen recently (an announcement every few minutes keeps them).
pub fn list() -> Vec<Found> {
    let mut s = state();
    s.found
        .retain(|f| f.seen.elapsed() < Duration::from_secs(600));
    let mut v = s.found.clone();
    v.sort_by_key(|f| f.name.to_lowercase());
    v
}
