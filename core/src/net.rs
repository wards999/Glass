//! Live-stream sessions over the LAN (unencrypted until M6).
//!
//! - Control: one TCP connection (Hello/Welcome, ping/pong clock sync,
//!   keyframe requests, pointer shape/visibility, Bye).
//!   LATER(M6): TLS 1.3 with pinned certificates.
//! - Video: UDP host -> client. The client first sends a "punch" datagram to the
//!   host's UDP port so the host learns where to send (and stateful firewalls
//!   on the client side let the replies in).
//! - Input: UDP client -> host on the same sockets (`input.rs`).
//!
//! The host side is driven by the app's capture/encode loop
//! ([`HostSession::send_frame`]) and injects input from a callback on its UDP
//! thread; the client side runs its own receive and control threads and hands
//! complete frames to one consumer thread through a fixed-size queue
//! ([`Client::wait_frame`]).
//! LATER(M3b): IPv6.

use crate::clock::{self, ClockSync};
use crate::input::{
    InputAction, InputApplier, InputCounters, InputPacket, InputSender, MAX_INPUT_DATAGRAM,
};
use crate::packet::{OutFrame, Packetiser, Reassembler, ReassemblyStats, MAX_DATAGRAM};
use crate::protocol::{
    decode_punch, encode_punch, Control, ControlError, CursorShape, Message, StreamInfo,
    CONTROL_MAX, FRAME_MAX, PROTOCOL_VERSION, PUNCH_SIZE, REJECT_VERSION,
};
use std::cell::UnsafeCell;
use std::io::{self, Read, Write};
use std::net::{
    Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// UDP socket buffers. A 1440p IDR is a burst of ~150 datagrams; 4 MB also
/// rides out short scheduling hiccups at 80-150 Mbps.
const SOCKET_BUFFER: usize = 4 << 20;
const PING_INTERVAL_US: u64 = 500_000;
/// Faster pings right after connecting so the clock offset settles quickly.
const FAST_PINGS: u32 = 8;
const KEYFRAME_RETRY_US: u64 = 250_000;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

fn udp_socket(bind: SocketAddrV4) -> io::Result<UdpSocket> {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    // Best effort: the OS may cap these.
    let _ = s.set_recv_buffer_size(SOCKET_BUFFER);
    let _ = s.set_send_buffer_size(SOCKET_BUFFER);
    s.bind(&SocketAddr::V4(bind).into())?;
    Ok(s.into())
}

fn send_control(w: &mut impl Write, msg: Control) -> io::Result<()> {
    let mut buf = [0u8; CONTROL_MAX];
    let n = msg.encode(&mut buf);
    w.write_all(&buf[..n])
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Accumulates TCP bytes and yields whole control messages. The buffer holds
/// the largest frame (a pointer image); it's allocated once per connection.
struct ControlReader {
    buf: Vec<u8>,
    len: usize,
}

impl ControlReader {
    fn new() -> Self {
        Self {
            buf: vec![0; FRAME_MAX + 1024],
            len: 0,
        }
    }

    /// Reads once (honouring the stream's read timeout) and calls `on_msg` for
    /// each complete message. `Ok(false)` = peer closed.
    fn read(
        &mut self,
        s: &mut TcpStream,
        mut on_msg: impl FnMut(Message<'_>) -> io::Result<()>,
    ) -> io::Result<bool> {
        match s.read(&mut self.buf[self.len..]) {
            Ok(0) => return Ok(false),
            Ok(n) => self.len += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(true)
            }
            Err(e) => return Err(e),
        }
        let mut used = 0;
        loop {
            match Message::decode(&self.buf[used..self.len]) {
                Ok((m, n)) => {
                    used += n;
                    on_msg(m)?;
                }
                Err(ControlError::Incomplete) => break,
                Err(ControlError::Malformed) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bad control message",
                    ))
                }
            }
        }
        self.buf.copy_within(used..self.len, 0);
        self.len -= used;
        Ok(true)
    }

    /// Blocks (up to the stream's read timeout) for exactly one small message.
    fn read_one(&mut self, s: &mut TcpStream) -> io::Result<Control> {
        let mut got = None;
        while got.is_none() {
            let open = self.read(s, |m| {
                if let Message::Control(c) = m {
                    got.get_or_insert(c);
                }
                Ok(())
            })?;
            if !open {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if got.is_none() && self.len == 0 {
                // Timed out with nothing buffered.
                return Err(io::ErrorKind::TimedOut.into());
            }
        }
        Ok(got.unwrap())
    }
}

fn pack_v4(a: SocketAddrV4) -> u64 {
    ((u32::from(*a.ip()) as u64) << 16) | a.port() as u64
}

fn unpack_v4(v: u64) -> Option<SocketAddrV4> {
    (v != 0).then(|| SocketAddrV4::new(Ipv4Addr::from((v >> 16) as u32), v as u16))
}

fn new_session_id() -> u32 {
    let t = clock::now_us();
    let id = (t ^ (t >> 29) ^ (std::process::id() as u64).rotate_left(17)) as u32;
    id.max(1)
}

// ---------------------------------------------------------------------------
// Host

/// Listens for viewers: TCP control and UDP media on the same port number.
pub struct HostListener {
    tcp: TcpListener,
    udp: UdpSocket,
}

/// Host-side counters for one session.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostCounters {
    pub frames_sent: u64,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub keyframe_requests: u32,
    pub input: InputCounters,
}

/// Called on the host's UDP thread for every input action, in order.
pub type InputSink = Box<dyn FnMut(InputAction) + Send>;

struct HostShared {
    stop: AtomicBool,
    connected: AtomicBool,
    keyframe: AtomicBool,
    keyframe_requests: AtomicU32,
    /// Client UDP address (IPv4 << 16 | port), 0 until the first punch.
    peer: AtomicU64,
    /// `InputApplier::applied_seq` for `FrameInfo::input_seq`.
    input_seq: AtomicU32,
    input: Mutex<InputCounters>,
}

/// One connected viewer. Dropping it ends the session (and releases any
/// keys/buttons the viewer was holding).
pub struct HostSession {
    shared: Arc<HostShared>,
    udp: UdpSocket,
    tcp_out: Arc<Mutex<TcpStream>>,
    packetiser: Packetiser,
    counters: HostCounters,
    cursor_buf: Vec<u8>,
    pub peer_tcp: SocketAddr,
    threads: Vec<JoinHandle<()>>,
}

impl HostListener {
    /// Binds `0.0.0.0:port` for TCP and UDP. `port = 0` picks a free one.
    pub fn bind(port: u16) -> io::Result<Self> {
        Self::bind_on(Ipv4Addr::UNSPECIFIED, port)
    }

    /// Like [`Self::bind`] on one interface (e.g. loopback-only for tests).
    pub fn bind_on(ip: Ipv4Addr, port: u16) -> io::Result<Self> {
        let tcp = TcpListener::bind((ip, port))?;
        let port = tcp.local_addr()?.port();
        let udp = udp_socket(SocketAddrV4::new(ip, port))?;
        Ok(Self { tcp, udp })
    }

    pub fn port(&self) -> u16 {
        self.tcp.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Waits for a viewer and completes the handshake. Input actions go to
    /// `on_input` (on the UDP thread). A failed handshake returns an error;
    /// call again to keep listening.
    pub fn accept(&self, info: StreamInfo, on_input: InputSink) -> io::Result<HostSession> {
        let (mut tcp, peer_tcp) = self.tcp.accept()?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let mut reader = ControlReader::new();
        match reader.read_one(&mut tcp)? {
            Control::Hello { version, .. } if version == PROTOCOL_VERSION => {}
            Control::Hello { .. } => {
                let _ = send_control(
                    &mut tcp,
                    Control::Reject {
                        reason: REJECT_VERSION,
                    },
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "viewer speaks a different protocol version",
                ));
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "expected Hello")),
        }
        let session_id = new_session_id();
        send_control(
            &mut tcp,
            Control::Welcome {
                version: PROTOCOL_VERSION,
                info,
                udp_port: self.udp.local_addr()?.port(),
                session_id,
            },
        )?;

        let shared = Arc::new(HostShared {
            stop: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            keyframe: AtomicBool::new(true),
            keyframe_requests: AtomicU32::new(0),
            peer: AtomicU64::new(0),
            input_seq: AtomicU32::new(0),
            input: Mutex::new(InputCounters::default()),
        });
        let tcp_out = Arc::new(Mutex::new(tcp.try_clone()?));
        let mut threads = Vec::new();

        // Control: answer pings, take keyframe requests, notice disconnects.
        tcp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let sh = shared.clone();
        let out = tcp_out.clone();
        threads.push(std::thread::spawn(move || {
            let mut reader = reader;
            while !sh.stop.load(Ordering::Relaxed) {
                let mut pong = None;
                let r = reader.read(&mut tcp, |m| {
                    match m {
                        Message::Control(Control::Ping { seq, client_ts }) => {
                            pong = Some(Control::Pong {
                                seq,
                                client_ts,
                                host_ts: clock::now_us(),
                            });
                        }
                        Message::Control(Control::KeyframeRequest) => {
                            sh.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                            sh.keyframe.store(true, Ordering::Relaxed);
                        }
                        Message::Control(Control::Bye) => {
                            return Err(io::ErrorKind::ConnectionAborted.into())
                        }
                        _ => {}
                    }
                    Ok(())
                });
                if !matches!(r, Ok(true)) {
                    break;
                }
                if let Some(p) = pong {
                    if send_control(&mut *lock(&out), p).is_err() {
                        break;
                    }
                }
            }
            let _ = send_control(&mut *lock(&out), Control::Bye);
            sh.connected.store(false, Ordering::Relaxed);
        }));

        // UDP from the viewer: punches (where to send video) and input.
        let udp = self.udp.try_clone()?;
        udp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let client_ip = peer_tcp.ip();
        let sh = shared.clone();
        let mut on_input = on_input;
        threads.push(std::thread::spawn(move || {
            let mut buf = [0u8; MAX_INPUT_DATAGRAM + 64];
            let mut applier = InputApplier::new();
            while !sh.stop.load(Ordering::Relaxed) && sh.connected.load(Ordering::Relaxed) {
                let Ok((n, from)) = udp.recv_from(&mut buf) else {
                    continue;
                };
                let (SocketAddr::V4(v4), true) = (from, from.ip() == client_ip) else {
                    continue;
                };
                let d = &buf[..n];
                if decode_punch(d) == Some(session_id) {
                    let packed = pack_v4(v4);
                    if sh.peer.swap(packed, Ordering::Relaxed) != packed {
                        // New destination: it needs a fresh IDR.
                        sh.keyframe.store(true, Ordering::Relaxed);
                    }
                } else if let Some(p) = InputPacket::decode(d) {
                    if p.session_id == session_id && applier.apply(&p, &mut on_input) {
                        sh.input_seq.store(applier.applied_seq, Ordering::Release);
                    }
                    *lock(&sh.input) = applier.counters;
                }
            }
            applier.release_all(&mut on_input);
            *lock(&sh.input) = applier.counters;
        }));

        Ok(HostSession {
            shared,
            udp: self.udp.try_clone()?,
            tcp_out,
            packetiser: Packetiser::default(),
            counters: HostCounters::default(),
            cursor_buf: Vec::with_capacity(FRAME_MAX),
            peer_tcp,
            threads,
        })
    }
}

impl HostSession {
    /// False once the viewer has gone (Bye, TCP closed or error).
    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    /// True once the viewer's UDP address is known (frames are being delivered).
    pub fn streaming(&self) -> bool {
        self.shared.peer.load(Ordering::Relaxed) != 0
    }

    /// Takes a pending keyframe request (viewer asked, or a new UDP peer).
    pub fn take_keyframe_request(&self) -> bool {
        self.shared.keyframe.swap(false, Ordering::Relaxed)
    }

    /// Latest input packet applied (stamp it on the next captured frame).
    pub fn input_seq(&self) -> u32 {
        self.shared.input_seq.load(Ordering::Acquire)
    }

    /// Sends one encoded frame. Returns `Ok(false)` (and sends nothing) while
    /// the viewer's UDP address is still unknown.
    pub fn send_frame(&mut self, f: &OutFrame, data: &[u8]) -> io::Result<bool> {
        let Some(peer) = unpack_v4(self.shared.peer.load(Ordering::Relaxed)) else {
            return Ok(false);
        };
        let udp = &self.udp;
        let c = &mut self.counters;
        let n = self.packetiser.send_frame(f, data, |pkt| {
            c.bytes_sent += pkt.len() as u64;
            udp.send_to(pkt, peer).map(|_| ())
        })?;
        c.packets_sent += n as u64;
        c.frames_sent += 1;
        Ok(true)
    }

    /// Sends a new pointer shape (call when it changes).
    pub fn send_cursor(&mut self, shape: &CursorShape) -> io::Result<()> {
        self.cursor_buf.clear();
        if !shape.encode(&mut self.cursor_buf) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad cursor"));
        }
        lock(&self.tcp_out).write_all(&self.cursor_buf)
    }

    /// Tells the viewer about a condition (`protocol::NOTICE_*`; `NOTICE_NONE` clears it).
    pub fn send_notice(&mut self, code: u8) -> io::Result<()> {
        send_control(&mut *lock(&self.tcp_out), Control::HostNotice { code })
    }

    pub fn send_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
        send_control(
            &mut *lock(&self.tcp_out),
            Control::CursorVisible { visible },
        )
    }

    pub fn counters(&self) -> HostCounters {
        HostCounters {
            keyframe_requests: self.shared.keyframe_requests.load(Ordering::Relaxed),
            input: *lock(&self.shared.input),
            ..self.counters
        }
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Client -> consumer frame queue

/// Per-frame metadata handed to the consumer. All `*_us` are local-clock except
/// `capture_ts_us` (host clock; see [`Client::host_to_local`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct LiveFrameMeta {
    pub frame_id: u32,
    pub flags: u8,
    pub packets: u16,
    pub encode_us: u32,
    /// Latest input packet the host had applied at capture (0 = none).
    pub input_seq: u32,
    pub capture_ts_us: u64,
    pub first_packet_us: u64,
    pub last_packet_us: u64,
    /// When the frame entered the consumer queue.
    pub queued_us: u64,
}

const QUEUE_LEN: usize = 8;

struct QueuedFrame {
    meta: LiveFrameMeta,
    data: Vec<u8>,
}

/// Single-producer single-consumer ring of preallocated frame buffers. The
/// mutex/condvar only park an idle consumer; data hand-off is lock-free.
struct FrameQueue {
    slots: Box<[UnsafeCell<QueuedFrame>]>,
    head: AtomicUsize, // next to consume
    tail: AtomicUsize, // next to fill
    park: Mutex<()>,
    cv: Condvar,
}

// SAFETY: slot `i` is written only by the producer while `i` is outside
// [head, tail), and read only by the consumer while inside it.
unsafe impl Sync for FrameQueue {}

impl FrameQueue {
    fn new() -> Self {
        Self {
            slots: (0..QUEUE_LEN)
                .map(|_| {
                    UnsafeCell::new(QueuedFrame {
                        meta: LiveFrameMeta::default(),
                        data: Vec::with_capacity(1 << 20),
                    })
                })
                .collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            park: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    /// Producer. False if full (the frame is dropped).
    fn push(&self, meta: LiveFrameMeta, data: &[u8]) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail - self.head.load(Ordering::Acquire) == QUEUE_LEN {
            return false;
        }
        // SAFETY: see `unsafe impl Sync`.
        let slot = unsafe { &mut *self.slots[tail % QUEUE_LEN].get() };
        slot.meta = meta;
        slot.data.clear();
        slot.data.extend_from_slice(data); // grows only past the largest frame so far
        self.tail.store(tail + 1, Ordering::Release);
        let _g = lock(&self.park);
        self.cv.notify_one();
        true
    }

    /// Consumer: waits up to `timeout` for the front frame (left in place
    /// until [`Self::pop`]).
    fn front(&self, timeout: Duration, alive: &AtomicBool) -> Option<&QueuedFrame> {
        let ready = |q: &Self| q.head.load(Ordering::Relaxed) != q.tail.load(Ordering::Acquire);
        if !ready(self) {
            let g = lock(&self.park);
            if !ready(self) && alive.load(Ordering::Relaxed) {
                let _ = self.cv.wait_timeout(g, timeout);
            }
        }
        if !ready(self) {
            return None;
        }
        let head = self.head.load(Ordering::Relaxed);
        // SAFETY: see `unsafe impl Sync`.
        Some(unsafe { &*self.slots[head % QUEUE_LEN].get() })
    }

    fn pop(&self) {
        let head = self.head.load(Ordering::Relaxed);
        if head != self.tail.load(Ordering::Acquire) {
            self.head.store(head + 1, Ordering::Release);
        }
    }

    fn wake(&self) {
        let _g = lock(&self.park);
        self.cv.notify_all();
    }
}

// ---------------------------------------------------------------------------
// Client

/// Why `Client::connect` failed. `repr(i32)` for the C ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ConnectError {
    /// Host name didn't resolve to an IPv4 address.
    Resolve = 1,
    /// TCP connect failed (refused, unreachable, timed out).
    Connect = 2,
    /// Connected but the handshake failed.
    Handshake = 3,
    /// Host speaks a different protocol version.
    Version = 4,
    /// Couldn't create the UDP socket.
    Socket = 5,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConnectError::Resolve => "host name did not resolve to an IPv4 address",
            ConnectError::Connect => {
                "could not connect (is the host running and allowed through its firewall?)"
            }
            ConnectError::Handshake => "connected, but the handshake failed",
            ConnectError::Version => "host runs a different Glass protocol version",
            ConnectError::Socket => "could not open a UDP socket",
        })
    }
}

impl std::error::Error for ConnectError {}

/// Client-side counters and clock state, safe to read from any thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct ClientStats {
    pub rx: ReassemblyStats,
    /// Complete frames dropped because the consumer queue was full.
    pub queue_full: u64,
    pub keyframe_requests: u32,
    pub connected: bool,
    pub clock_valid: bool,
    /// `host clock - local clock`, µs (valid when `clock_valid`).
    pub clock_offset_us: i64,
    /// RTT of the sample behind the offset (error bound ±rtt/2), and the latest.
    pub clock_rtt_us: u32,
    pub last_rtt_us: u32,
    /// Input events (keys, buttons, scrolls) and datagrams sent.
    pub input_events: u64,
    pub input_packets: u64,
    /// Latest `HostNotice` code (`protocol::NOTICE_*`), 0 = none.
    pub host_notice: u8,
}

/// The host's pointer as last reported. `generation` changes on every update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct CursorInfo {
    pub generation: u32,
    pub visible: bool,
    /// 0 until the host has sent a shape.
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
}

#[derive(Default)]
struct CursorState {
    info: CursorInfo,
    rgba: Vec<u8>,
}

#[derive(Default)]
struct AtomicRx {
    v: [AtomicU64; 8],
}

impl AtomicRx {
    fn store(&self, s: &ReassemblyStats) {
        let a = [
            s.packets,
            s.bytes,
            s.duplicates,
            s.late,
            s.bad,
            s.frames,
            s.frames_lost,
            s.frames_skipped,
        ];
        for (slot, v) in self.v.iter().zip(a) {
            slot.store(v, Ordering::Relaxed);
        }
    }

    fn load(&self) -> ReassemblyStats {
        let l = |i: usize| self.v[i].load(Ordering::Relaxed);
        ReassemblyStats {
            packets: l(0),
            bytes: l(1),
            duplicates: l(2),
            late: l(3),
            bad: l(4),
            frames: l(5),
            frames_lost: l(6),
            frames_skipped: l(7),
        }
    }
}

/// Input side of the client: builds datagrams and sends them on the media
/// socket. Shared by the UI thread (events) and the control thread (heartbeats).
struct InputTx {
    sender: InputSender,
    udp: UdpSocket,
    dest: SocketAddrV4,
    buf: [u8; MAX_INPUT_DATAGRAM],
    packets: u64,
    active: bool, // any input yet (no heartbeats before the first event)
}

impl InputTx {
    fn flush(&mut self, now: u64) {
        let (n, _) = self.sender.build(now, &mut self.buf);
        let _ = self.udp.send_to(&self.buf[..n], self.dest);
        self.packets += 1;
        self.active = true;
    }
}

struct ClientShared {
    stop: AtomicBool,
    connected: AtomicBool,
    want_keyframe: AtomicBool,
    keyframe_requests: AtomicU32,
    queue_full: AtomicU64,
    rx: AtomicRx,
    clock_valid: AtomicBool,
    clock_offset: AtomicI64,
    clock_rtt: AtomicU32,
    last_rtt: AtomicU32,
    host_notice: std::sync::atomic::AtomicU8,
    queue: FrameQueue,
    input: Mutex<InputTx>,
    cursor: Mutex<CursorState>,
}

/// A viewer connection. Exactly one thread may call `wait_frame`/`release_frame`.
/// Input methods may be called from any thread.
pub struct Client {
    shared: Arc<ClientShared>,
    info: StreamInfo,
    host: SocketAddrV4,
    threads: Vec<JoinHandle<()>>,
}

/// Result of [`Client::wait_frame`].
pub enum Wait<'a> {
    Frame(LiveFrameMeta, &'a [u8]),
    Timeout,
    Disconnected,
}

impl Client {
    /// Connects to `host:port`, completes the handshake and starts receiving.
    pub fn connect(host: &str, port: u16) -> Result<Self, ConnectError> {
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|_| ConnectError::Resolve)?
            .find_map(|a| match a {
                SocketAddr::V4(v4) => Some(v4),
                SocketAddr::V6(_) => None,
            })
            .ok_or(ConnectError::Resolve)?;
        let mut tcp = TcpStream::connect_timeout(&SocketAddr::V4(addr), Duration::from_secs(3))
            .map_err(|_| ConnectError::Connect)?;
        let hs = |_| ConnectError::Handshake;
        tcp.set_nodelay(true).map_err(hs)?;
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).map_err(hs)?;
        send_control(
            &mut tcp,
            Control::Hello {
                version: PROTOCOL_VERSION,
                display_w: 0, // LATER(M7): request native resolution.
                display_h: 0,
            },
        )
        .map_err(hs)?;
        let mut reader = ControlReader::new();
        let (info, udp_port, session_id) = match reader.read_one(&mut tcp).map_err(hs)? {
            Control::Welcome {
                version,
                info,
                udp_port,
                session_id,
            } if version == PROTOCOL_VERSION => (info, udp_port, session_id),
            Control::Welcome { .. } | Control::Reject { .. } => return Err(ConnectError::Version),
            _ => return Err(ConnectError::Handshake),
        };
        let host_udp = SocketAddrV4::new(*addr.ip(), udp_port);
        let sock = |_| ConnectError::Socket;
        let udp = udp_socket(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).map_err(sock)?;
        udp.set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(sock)?;
        let punch = encode_punch(session_id);
        let _ = udp.send_to(&punch, host_udp);
        tcp.set_read_timeout(Some(Duration::from_millis(20)))
            .map_err(hs)?;

        let shared = Arc::new(ClientShared {
            stop: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            want_keyframe: AtomicBool::new(false),
            keyframe_requests: AtomicU32::new(0),
            queue_full: AtomicU64::new(0),
            rx: AtomicRx::default(),
            clock_valid: AtomicBool::new(false),
            clock_offset: AtomicI64::new(0),
            clock_rtt: AtomicU32::new(0),
            last_rtt: AtomicU32::new(0),
            host_notice: std::sync::atomic::AtomicU8::new(0),
            queue: FrameQueue::new(),
            input: Mutex::new(InputTx {
                sender: InputSender::new(session_id),
                udp: udp.try_clone().map_err(sock)?,
                dest: host_udp,
                buf: [0; MAX_INPUT_DATAGRAM],
                packets: 0,
                active: false,
            }),
            cursor: Mutex::new(CursorState::default()),
        });
        let threads = vec![
            spawn_receiver(shared.clone(), udp, host_udp, punch),
            spawn_control(shared.clone(), tcp, reader),
        ];
        Ok(Self {
            shared,
            info,
            host: addr,
            threads,
        })
    }

    pub fn info(&self) -> StreamInfo {
        self.info
    }

    pub fn host(&self) -> SocketAddrV4 {
        self.host
    }

    /// Waits up to `timeout` for the next frame. The returned bytes stay valid
    /// until [`Self::release_frame`]; call it before waiting again.
    pub fn wait_frame(&self, timeout: Duration) -> Wait<'_> {
        let sh = &self.shared;
        match sh.queue.front(timeout, &sh.connected) {
            Some(f) => Wait::Frame(f.meta, &f.data),
            None if !sh.connected.load(Ordering::Relaxed) => Wait::Disconnected,
            None => Wait::Timeout,
        }
    }

    pub fn release_frame(&self) {
        self.shared.queue.pop();
    }

    /// Ask the host for an IDR (e.g. after a decode error).
    pub fn request_keyframe(&self) {
        self.shared.want_keyframe.store(true, Ordering::Relaxed);
    }

    // --- input (sent at once; any thread) ---

    fn input(&self, f: impl FnOnce(&mut InputSender, u64)) {
        let now = clock::now_us();
        let mut tx = lock(&self.shared.input);
        f(&mut tx.sender, now);
        tx.flush(now);
    }

    /// Key down (repeat downs for auto-repeat) or up. `hid` from `keymap`.
    pub fn input_key(&self, hid: u8, down: bool) {
        self.input(|s, now| s.key(now, hid, down));
    }

    /// Button down/up at normalised position (0..=65535).
    pub fn input_button(&self, button: u8, down: bool, x: u16, y: u16) {
        self.input(|s, now| s.button(now, button, down, x, y));
    }

    pub fn input_move(&self, x: u16, y: u16) {
        self.input(|s, _| s.move_to(x, y));
    }

    /// Wheel units (120 = one notch); `dy > 0` up, `dx > 0` right.
    pub fn input_scroll(&self, dy: i16, dx: i16) {
        self.input(|s, now| s.scroll(now, dy, dx));
    }

    /// Lets go of everything held (focus lost).
    pub fn input_release_all(&self) {
        self.input(|s, now| s.release_all(now));
    }

    /// When input packet `seq` was sent (local µs), for input -> screen.
    pub fn input_sent_at(&self, seq: u32) -> Option<u64> {
        lock(&self.shared.input).sender.sent_at(seq)
    }

    /// The host's pointer. Copies the image into `rgba` (resized) if
    /// `generation` differs from `seen`; returns the info either way.
    pub fn cursor(&self, seen: u32, rgba: &mut Vec<u8>) -> CursorInfo {
        let c = lock(&self.shared.cursor);
        if c.info.generation != seen {
            rgba.clear();
            rgba.extend_from_slice(&c.rgba);
        }
        c.info
    }

    pub fn stats(&self) -> ClientStats {
        let sh = &self.shared;
        let (events, packets) = {
            let tx = lock(&sh.input);
            (tx.sender.events_total, tx.packets)
        };
        ClientStats {
            rx: sh.rx.load(),
            queue_full: sh.queue_full.load(Ordering::Relaxed),
            keyframe_requests: sh.keyframe_requests.load(Ordering::Relaxed),
            connected: sh.connected.load(Ordering::Relaxed),
            clock_valid: sh.clock_valid.load(Ordering::Acquire),
            clock_offset_us: sh.clock_offset.load(Ordering::Relaxed),
            clock_rtt_us: sh.clock_rtt.load(Ordering::Relaxed),
            last_rtt_us: sh.last_rtt.load(Ordering::Relaxed),
            input_events: events,
            input_packets: packets,
            host_notice: sh.host_notice.load(Ordering::Relaxed),
        }
    }

    /// Maps a host timestamp to the local clock, once clock sync has a sample.
    pub fn host_to_local(&self, host_us: u64) -> Option<u64> {
        let sh = &self.shared;
        sh.clock_valid
            .load(Ordering::Acquire)
            .then(|| clock::host_to_local(host_us, sh.clock_offset.load(Ordering::Relaxed)))
    }
}

/// Datagrams -> reassembler -> queue. Re-punches every second until video
/// flows, then every 5 s.
fn spawn_receiver(
    sh: Arc<ClientShared>,
    udp: UdpSocket,
    host_udp: SocketAddrV4,
    punch: [u8; PUNCH_SIZE],
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut r = Reassembler::new();
        let mut buf = [0u8; MAX_DATAGRAM + 64];
        let mut last_punch = clock::now_us();
        let host_ip = std::net::IpAddr::V4(*host_udp.ip());
        while !sh.stop.load(Ordering::Relaxed) && sh.connected.load(Ordering::Relaxed) {
            let now = clock::now_us();
            let every = if r.stats.packets == 0 {
                1_000_000
            } else {
                5_000_000
            };
            if now.saturating_sub(last_punch) >= every {
                let _ = udp.send_to(&punch, host_udp);
                last_punch = now;
            }
            let Ok((n, from)) = udp.recv_from(&mut buf) else {
                continue;
            };
            if from.ip() != host_ip {
                continue;
            }
            if let Some(f) = r.push(&buf[..n], clock::now_us()) {
                let meta = LiveFrameMeta {
                    frame_id: f.frame_id,
                    flags: f.flags,
                    packets: f.packets,
                    encode_us: f.encode_us,
                    input_seq: f.input_seq,
                    capture_ts_us: f.capture_ts_us,
                    first_packet_us: f.first_packet_us,
                    last_packet_us: f.last_packet_us,
                    queued_us: clock::now_us(),
                };
                if !sh.queue.push(meta, f.data) {
                    sh.queue_full.fetch_add(1, Ordering::Relaxed);
                    r.resync();
                }
            }
            // Ask only once something was actually lost or skipped: at start-up
            // the first IDR is usually just still arriving.
            let s = &r.stats;
            let damaged = s.frames_lost + s.frames_skipped + s.bad > 0
                || sh.queue_full.load(Ordering::Relaxed) > 0;
            if r.needs_keyframe() && damaged {
                sh.want_keyframe.store(true, Ordering::Relaxed);
            }
            sh.rx.store(&r.stats);
        }
        sh.queue.wake();
    })
}

/// Pings for clock sync, keyframe requests, input heartbeats, pointer
/// updates, disconnect detection.
fn spawn_control(
    sh: Arc<ClientShared>,
    mut tcp: TcpStream,
    mut reader: ControlReader,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut sync = ClockSync::default();
        let mut seq = 0u32;
        let mut last_ping = 0u64;
        let mut last_kf = 0u64;
        loop {
            if sh.stop.load(Ordering::Relaxed) {
                let _ = send_control(&mut tcp, Control::Bye);
                break;
            }
            let now = clock::now_us();
            let interval = if seq < FAST_PINGS {
                50_000
            } else {
                PING_INTERVAL_US
            };
            let mut ok = true;
            if now.saturating_sub(last_ping) >= interval {
                seq += 1;
                last_ping = now;
                let ping = Control::Ping {
                    seq,
                    client_ts: clock::now_us(),
                };
                ok &= send_control(&mut tcp, ping).is_ok();
            }
            if now.saturating_sub(last_kf) >= KEYFRAME_RETRY_US
                && sh.want_keyframe.swap(false, Ordering::Relaxed)
            {
                last_kf = now;
                sh.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                ok &= send_control(&mut tcp, Control::KeyframeRequest).is_ok();
            }
            {
                let mut tx = lock(&sh.input);
                if tx.active && tx.sender.heartbeat_due(now) {
                    tx.flush(now);
                }
            }
            let r = reader.read(&mut tcp, |m| {
                match m {
                    Message::Control(Control::Pong {
                        client_ts, host_ts, ..
                    }) => {
                        sync.add(client_ts, host_ts, clock::now_us());
                        if let (Some(off), Some(rtt)) = (sync.offset_us(), sync.best_rtt_us()) {
                            sh.clock_offset.store(off, Ordering::Relaxed);
                            sh.clock_rtt.store(rtt, Ordering::Relaxed);
                            let last = sync.last_rtt_us().unwrap_or(rtt);
                            sh.last_rtt.store(last, Ordering::Relaxed);
                            sh.clock_valid.store(true, Ordering::Release);
                        }
                    }
                    Message::Control(Control::HostNotice { code }) => {
                        sh.host_notice.store(code, Ordering::Relaxed);
                    }
                    Message::Control(Control::CursorVisible { visible }) => {
                        let mut c = lock(&sh.cursor);
                        c.info.visible = visible;
                        c.info.generation = c.info.generation.wrapping_add(1);
                    }
                    Message::Cursor(shape) => {
                        let mut c = lock(&sh.cursor);
                        c.rgba.clear();
                        c.rgba.extend_from_slice(shape.rgba);
                        c.info = CursorInfo {
                            generation: c.info.generation.wrapping_add(1),
                            visible: c.info.visible || c.info.width == 0,
                            width: shape.width,
                            height: shape.height,
                            hot_x: shape.hot_x,
                            hot_y: shape.hot_y,
                        };
                    }
                    Message::Control(Control::Bye) => {
                        return Err(io::ErrorKind::ConnectionAborted.into())
                    }
                    _ => {}
                }
                Ok(())
            });
            if !ok || !matches!(r, Ok(true)) {
                break;
            }
        }
        sh.connected.store(false, Ordering::Relaxed);
        sh.queue.wake();
    })
}

impl Drop for Client {
    fn drop(&mut self) {
        // Let go of anything held before saying goodbye.
        {
            let mut tx = lock(&self.shared.input);
            if tx.active {
                let now = clock::now_us();
                tx.sender.release_all(now);
                tx.flush(now);
            }
        }
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.queue.wake();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
