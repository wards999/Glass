//! This PC's Wi-Fi link during a session (M4 step 5): signal, rate,
//! channel and the access point, sampled once a second by the WLAN API (no
//! admin rights) and tracked by core (`spikes::LinkTrack`), so the results
//! say "Wi-Fi here: …" the same way on both platforms. Read only: nothing
//! is ever changed.

use glass_core::spikes::{LinkSample, LinkTrack};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::{
    wlan_interface_state_connected, wlan_intf_opcode_channel_number,
    wlan_intf_opcode_current_connection, wlan_intf_opcode_rssi, WlanCloseHandle,
    WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle, WlanQueryInterface,
    WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST,
};

/// Samples the link once a second until dropped.
pub struct WifiWatch {
    track: Arc<Mutex<LinkTrack>>,
    stop: Arc<AtomicBool>,
}

impl WifiWatch {
    pub fn start() -> Self {
        let track = Arc::new(Mutex::new(LinkTrack::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t, s) = (track.clone(), stop.clone());
        let _ = std::thread::Builder::new()
            .name("glass-wifi".into())
            .spawn(move || {
                let client = Client::open();
                while !s.load(Ordering::Relaxed) {
                    let sample = client.as_ref().map(Client::sample).unwrap_or_default();
                    if let Ok(mut t) = t.lock() {
                        t.feed(&sample);
                    }
                    for _ in 0..10 {
                        if s.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            });
        Self { track, stop }
    }

    /// "Wi-Fi here: signal … | rate … | roams …".
    pub fn line(&self) -> String {
        self.track
            .lock()
            .map(|t| t.describe())
            .unwrap_or_else(|_| "Wi-Fi here: not measured".into())
    }
}

impl Drop for WifiWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Client(HANDLE);

impl Client {
    fn open() -> Option<Self> {
        let mut version = 0u32;
        let mut h = HANDLE::default();
        // SAFETY: plain WLAN API call; the handle is closed in Drop.
        (unsafe { WlanOpenHandle(2, None, &mut version, &mut h) } == 0).then_some(Self(h))
    }

    /// The first connected Wi-Fi interface's link, or `wifi: false`.
    fn sample(&self) -> LinkSample {
        let mut out = LinkSample::default();
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        // SAFETY: the list and every query result are freed with
        // WlanFreeMemory; sizes are checked before reading.
        unsafe {
            if WlanEnumInterfaces(self.0, None, &mut list) != 0 || list.is_null() {
                return out;
            }
            let n = (*list).dwNumberOfItems as usize;
            let items = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), n);
            if let Some(i) = items
                .iter()
                .find(|i| i.isState == wlan_interface_state_connected)
            {
                let guid = i.InterfaceGuid;
                let query = |op| -> Option<(*mut core::ffi::c_void, u32)> {
                    let mut size = 0u32;
                    let mut data = std::ptr::null_mut();
                    (WlanQueryInterface(self.0, &guid, op, None, &mut size, &mut data, None) == 0
                        && !data.is_null())
                    .then_some((data, size))
                };
                if let Some((d, size)) = query(wlan_intf_opcode_current_connection) {
                    if size as usize >= std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
                        let a =
                            &(*d.cast::<WLAN_CONNECTION_ATTRIBUTES>()).wlanAssociationAttributes;
                        out.wifi = true;
                        out.quality_pct = a.wlanSignalQuality;
                        out.rate_mbps = a.ulTxRate / 1_000; // kbps
                        out.bssid = a.dot11Bssid;
                    }
                    WlanFreeMemory(d);
                }
                if let Some((d, size)) = query(wlan_intf_opcode_rssi) {
                    if size >= 4 {
                        out.rssi_dbm = *d.cast::<i32>();
                    }
                    WlanFreeMemory(d);
                }
                if let Some((d, size)) = query(wlan_intf_opcode_channel_number) {
                    if size >= 4 {
                        out.channel = *d.cast::<u32>();
                    }
                    WlanFreeMemory(d);
                }
            }
            WlanFreeMemory(list.cast());
        }
        out
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: the handle `open` got.
        unsafe {
            WlanCloseHandle(self.0, None);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn samples_this_pc() {
        if let Some(c) = super::Client::open() {
            let s = c.sample();
            println!("{s:?}");
            if s.wifi {
                assert!(s.rssi_dbm < 0 && s.quality_pct <= 100, "{s:?}");
            }
        }
    }
}
