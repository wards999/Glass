//! Which of this PC's addresses to advertise (M4 step 4, SPEC §6
//! "Pick the right interface"). PCs have extra adapters: VPNs (Tailscale,
//! WireGuard), Hyper-V/WSL virtual switches, Bluetooth, link-local
//! 169.254.x. Viewers connect to the address in the mDNS record, so it must
//! be the real LAN one.
//!
//! Picks an up Ethernet or Wi-Fi adapter with an IPv4 default gateway,
//! lowest route metric first. VPNs are tunnel or virtual adapter types, and
//! WSL's virtual switch has no gateway, so they're left out by kind, not by
//! name.

use std::net::Ipv4Addr;
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
    GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

/// The LAN address to advertise, if an adapter qualifies.
pub fn lan_ipv4() -> Option<Ipv4Addr> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 16 * 1024u32;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buf = vec![0; size as usize / 8 + 1];
        // SAFETY: `buf` is `size` bytes, 8-aligned.
        let r = unsafe {
            GetAdaptersAddresses(
                AF_INET.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &mut size,
            )
        };
        match r {
            0 => break,
            111 => continue, // ERROR_BUFFER_OVERFLOW: `size` is now enough
            _ => return None,
        }
    }
    let mut best: Option<(u32, Ipv4Addr)> = None;
    let mut a = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // SAFETY: walking the list GetAdaptersAddresses wrote into `buf`.
    unsafe {
        while !a.is_null() {
            let ad = &*a;
            a = ad.Next;
            let kind_ok = ad.IfType == IF_TYPE_ETHERNET_CSMACD || ad.IfType == IF_TYPE_IEEE80211;
            if !kind_ok || ad.OperStatus != IfOperStatusUp || ad.FirstGatewayAddress.is_null() {
                continue;
            }
            let mut u = ad.FirstUnicastAddress;
            while !u.is_null() {
                let sa = (*u).Address.lpSockaddr;
                u = (*u).Next;
                if sa.is_null() || (*sa).sa_family != AF_INET {
                    continue;
                }
                let sin = &*sa.cast::<SOCKADDR_IN>();
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                if ip.is_link_local() || ip.is_loopback() || ip.is_unspecified() {
                    continue;
                }
                if best.is_none_or(|(m, _)| ad.Ipv4Metric < m) {
                    best = Some((ad.Ipv4Metric, ip));
                }
                break;
            }
        }
    }
    best.map(|(_, ip)| ip)
}

#[cfg(test)]
mod tests {
    #[test]
    fn picks_a_real_lan_address() {
        let ip = super::lan_ipv4();
        println!("lan_ipv4: {ip:?}");
        if let Some(ip) = ip {
            assert!(!ip.is_link_local() && !ip.is_loopback());
        }
    }
}
