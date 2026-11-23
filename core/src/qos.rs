//! Packet marking (M4 step 4, SPEC §6b "Packet marking"): video as video
//! (Wi-Fi's AC_VI queue), input as voice (AC_VO), so the machines' own Wi-Fi
//! drivers, and WMM routers that honour DSCP, send them ahead of a file
//! copy or a download. Where marking isn't honoured it changes nothing.
//!
//! - DSCP via `IP_TOS` everywhere (video AF41 = 34, input EF = 46).
//! - macOS: `SO_NET_SERVICE_TYPE` (video `RV`, input `VO`): the Mac's own
//!   Wi-Fi driver queues by it.
//! - Windows ignores `IP_TOS` from programs. qWAVE's `QOSAddSocketToFlow`
//!   with the `AudioVideo` / `Voice` traffic types marks the packets
//!   (DSCP and 802.1p, so the Wi-Fi driver picks the queue) without admin
//!   rights. It needs the destination address.

use std::net::{SocketAddrV4, UdpSocket};

/// What a socket mostly sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Host -> viewer: video (and its resends and keep-awakes).
    Video,
    /// Viewer -> host: input, NACKs, keep-awakes.
    Input,
}

impl Class {
    fn dscp(self) -> u32 {
        match self {
            Class::Video => 34, // AF41
            Class::Input => 46, // EF
        }
    }
}

/// The result, for the session results (`HostCounters::qos`,
/// `ClientStats::qos`): 0 not tried yet, 1 marked, 2 the OS refused.
pub const QOS_NONE: u8 = 0;
pub const QOS_MARKED: u8 = 1;
pub const QOS_REFUSED: u8 = 2;

/// Marking on one socket towards one destination; undone when dropped.
pub struct Marking {
    #[cfg(windows)]
    _win: Option<win::Flow>,
    /// `QOS_MARKED` or `QOS_REFUSED`.
    pub state: u8,
}

/// Marks what `udp` sends to `dest` as `class`.
pub fn mark(udp: &UdpSocket, class: Class, dest: SocketAddrV4) -> Marking {
    let tos_ok = socket2::SockRef::from(udp)
        .set_tos_v4(class.dscp() << 2)
        .is_ok();
    #[cfg(target_os = "macos")]
    {
        let _ = dest;
        let ok = mac::set_service_type(udp, class);
        Marking {
            state: if ok || tos_ok {
                QOS_MARKED
            } else {
                QOS_REFUSED
            },
        }
    }
    #[cfg(windows)]
    {
        let _ = tos_ok; // ignored by Windows unless policy allows it
        let flow = win::Flow::add(udp, class, dest);
        Marking {
            state: if flow.is_some() {
                QOS_MARKED
            } else {
                QOS_REFUSED
            },
            _win: flow,
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = dest;
        Marking {
            state: if tos_ok { QOS_MARKED } else { QOS_REFUSED },
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::Class;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;

    // <sys/socket.h>
    const SOL_SOCKET: i32 = 0xffff;
    const SO_NET_SERVICE_TYPE: i32 = 0x1116;
    const NET_SERVICE_TYPE_VO: i32 = 4;
    const NET_SERVICE_TYPE_RV: i32 = 5;

    extern "C" {
        fn setsockopt(
            fd: i32,
            level: i32,
            name: i32,
            value: *const core::ffi::c_void,
            len: u32,
        ) -> i32;
    }

    pub fn set_service_type(udp: &UdpSocket, class: Class) -> bool {
        let v: i32 = match class {
            Class::Video => NET_SERVICE_TYPE_RV,
            Class::Input => NET_SERVICE_TYPE_VO,
        };
        // SAFETY: a valid socket and a 4-byte int option.
        unsafe {
            setsockopt(
                udp.as_raw_fd(),
                SOL_SOCKET,
                SO_NET_SERVICE_TYPE,
                (&v as *const i32).cast(),
                4,
            ) == 0
        }
    }
}

#[cfg(windows)]
mod win {
    use super::Class;
    use std::net::{SocketAddrV4, UdpSocket};
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::NetworkManagement::QoS::{
        QOSAddSocketToFlow, QOSCloseHandle, QOSCreateHandle, QOSRemoveSocketFromFlow,
        QOSTrafficTypeAudioVideo, QOSTrafficTypeVoice, QOS_NON_ADAPTIVE_FLOW, QOS_VERSION,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, IN_ADDR, IN_ADDR_0, SOCKADDR, SOCKADDR_IN, SOCKET,
    };

    pub struct Flow {
        handle: windows_sys::Win32::Foundation::HANDLE,
        socket: SOCKET,
        id: u32,
    }

    // The handle is only used to undo the marking, from whichever thread
    // drops it.
    unsafe impl Send for Flow {}
    unsafe impl Sync for Flow {}

    impl Flow {
        pub fn add(udp: &UdpSocket, class: Class, dest: SocketAddrV4) -> Option<Self> {
            let version = QOS_VERSION {
                MajorVersion: 1,
                MinorVersion: 0,
            };
            let mut handle = std::ptr::null_mut();
            // SAFETY: plain qWAVE calls with valid pointers; the handle is
            // closed in Drop (or here on failure).
            unsafe {
                if QOSCreateHandle(&version, &mut handle) == 0 {
                    return None;
                }
                let addr = SOCKADDR_IN {
                    sin_family: AF_INET,
                    sin_port: dest.port().to_be(),
                    sin_addr: IN_ADDR {
                        S_un: IN_ADDR_0 {
                            S_addr: u32::from_ne_bytes(dest.ip().octets()),
                        },
                    },
                    sin_zero: [0; 8],
                };
                let kind = match class {
                    Class::Video => QOSTrafficTypeAudioVideo,
                    Class::Input => QOSTrafficTypeVoice,
                };
                let socket = udp.as_raw_socket() as SOCKET;
                let mut id = 0u32;
                let ok = QOSAddSocketToFlow(
                    handle,
                    socket,
                    (&addr as *const SOCKADDR_IN).cast::<SOCKADDR>(),
                    kind,
                    QOS_NON_ADAPTIVE_FLOW,
                    &mut id,
                );
                if ok == 0 {
                    QOSCloseHandle(handle);
                    return None;
                }
                Some(Self { handle, socket, id })
            }
        }
    }

    impl Drop for Flow {
        fn drop(&mut self) {
            // SAFETY: the flow and handle this struct created.
            unsafe {
                QOSRemoveSocketFromFlow(self.handle, self.socket, self.id, 0);
                QOSCloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing is sent: marking only configures the socket.
    #[test]
    fn marking_is_accepted_or_refused_never_silent() {
        let udp = UdpSocket::bind("0.0.0.0:0").unwrap();
        let dest: SocketAddrV4 = "192.168.4.1:9".parse().unwrap();
        for class in [Class::Video, Class::Input] {
            let m = mark(&udp, class, dest);
            println!("{class:?}: state {}", m.state);
            assert_ne!(m.state, QOS_NONE);
        }
    }
}
