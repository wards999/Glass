//! Live-stream sessions over the LAN, paired devices only (M6).
//!
//! - Control: one TCP connection inside mutual TLS 1.3 (`tls.rs`). Each side
//!   checks the other's certificate fingerprint against its paired devices
//!   (`identity.rs`) before acting on anything. An unknown viewer may only
//!   pair (PIN, `pairing.rs`); it can't get a session.
//!   Messages: Hello/Welcome, ping/pong clock sync, keyframe requests,
//!   pointer shape/visibility, notices, Bye.
//! - Video: UDP host -> viewer. The viewer first sends a "punch" datagram to
//!   the host's UDP port so the host learns where to send (and stateful
//!   firewalls on the viewer side let the replies in).
//! - Input: UDP viewer -> host on the same sockets (`input.rs`).
//! - Every UDP datagram is AES-128-GCM sealed (`seal.rs`) with per-session,
//!   per-direction keys from the TLS exporter, so only the paired peer in
//!   this session can produce or read them.
//!
//! The host side is driven by the app's capture/encode loop
//! ([`HostSession::send_frame`]) and injects input from a callback on its UDP
//! thread; the client side runs its own receive and control threads and hands
//! complete frames to one consumer thread through a fixed-size queue
//! ([`Client::wait_frame`]).
//! LATER(M3b): IPv6. LATER(M7): more than one viewer at a time.

use crate::clock::{self, ClockSync};
use crate::identity::{
    clean_name, fingerprint_short, Device, Fingerprint, Peer, ROLE_HOST, ROLE_VIEWER,
};
use crate::input::{
    InputAction, InputApplier, InputCounters, InputPacket, InputSender, MAX_INPUT_DATAGRAM,
};
use crate::packet::{OutFrame, Packetiser, Reassembler, ReassemblyStats};
use crate::pairing::{self, PinState};
use crate::protocol::{
    decode_punch, encode_pair, encode_punch, Control, ControlError, CursorShape, Message,
    StreamInfo, CONTROL_MAX, FRAME_MAX, PAIR_CONFIRM, PAIR_PAKE, PAIR_READY, PAIR_START,
    PROTOCOL_VERSION, PUNCH_SIZE, REJECT_DENIED, REJECT_NOT_PAIRED, REJECT_PAIRING_LOCKED,
    REJECT_VERSION, REJECT_WRONG_PIN,
};
use crate::seal::{Opener, Sealer, Secret, WIRE_MAX};
use crate::tls::{self, Channel, TlsReader, TlsWriter};
use std::cell::UnsafeCell;
use std::io::{self, Read};
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
/// How long a viewer waits for Welcome (the host may be asking its user).
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
/// TLS exporter label for the media keys (context = session id).
const MEDIA_LABEL: &[u8] = b"EXPORTER-glass-media";

fn udp_socket(bind: SocketAddrV4) -> io::Result<UdpSocket> {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    // Best effort: the OS may cap these.
    let _ = s.set_recv_buffer_size(SOCKET_BUFFER);
    let _ = s.set_send_buffer_size(SOCKET_BUFFER);
    s.bind(&SocketAddr::V4(bind).into())?;
    Ok(s.into())
}

fn send_control(w: &TlsWriter, msg: Control) -> io::Result<()> {
    let mut buf = [0u8; CONTROL_MAX];
    let n = msg.encode(&mut buf);
    w.write_all(&buf[..n])
}

fn send_pair(w: &TlsWriter, kind: u8, body: &[u8]) -> io::Result<()> {
    let mut b = Vec::with_capacity(body.len() + 3);
    if !encode_pair(kind, body, &mut b) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad pairing message",
        ));
    }
    w.write_all(&b)
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// (host -> viewer, viewer -> host) media secrets for this session.
fn media_secrets(w: &TlsWriter, session_id: u32) -> io::Result<(Secret, Secret)> {
    let mut out = [0u8; 64];
    w.export(MEDIA_LABEL, &session_id.to_be_bytes(), &mut out)?;
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    a.copy_from_slice(&out[..32]);
    b.copy_from_slice(&out[32..]);
    Ok((a, b))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// A handshake-time message, owned.
enum Incoming {
    Control(Control),
    Pair(u8, Vec<u8>),
}

/// Accumulates control bytes and yields whole messages. The buffer holds
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

    /// `Ok(false)` = peer closed. Timeouts come back as errors.
    fn read_more(&mut self, s: &mut impl Read) -> io::Result<bool> {
        let n = s.read(&mut self.buf[self.len..])?;
        self.len += n;
        Ok(n > 0)
    }

    fn drain(&mut self, on_msg: &mut impl FnMut(Message<'_>) -> io::Result<()>) -> io::Result<()> {
        let mut used = 0;
        loop {
            match Message::decode(&self.buf[used..self.len]) {
                Ok((m, n)) => {
                    used += n;
                    on_msg(m)?;
                }
                Err(ControlError::Incomplete) => break,
                Err(ControlError::Malformed) => return Err(invalid("bad control message")),
            }
        }
        self.buf.copy_within(used..self.len, 0);
        self.len -= used;
        Ok(())
    }

    /// Reads once (honouring the stream's read timeout) and calls `on_msg` for
    /// each complete message. `Ok(false)` = peer closed.
    fn read(
        &mut self,
        s: &mut impl Read,
        mut on_msg: impl FnMut(Message<'_>) -> io::Result<()>,
    ) -> io::Result<bool> {
        // Anything left over from the handshake first.
        self.drain(&mut on_msg)?;
        match self.read_more(s) {
            Ok(false) => return Ok(false),
            Ok(true) => {}
            Err(e) if is_timeout(&e) => return Ok(true),
            Err(e) => return Err(e),
        }
        self.drain(&mut on_msg)?;
        Ok(true)
    }

    /// Blocks (up to the stream's read timeout per read) for exactly one
    /// message, leaving anything after it buffered. Pointer shapes are skipped.
    fn read_one(&mut self, s: &mut impl Read) -> io::Result<Incoming> {
        loop {
            let got = match Message::decode(&self.buf[..self.len]) {
                Ok((m, n)) => {
                    let got = match m {
                        Message::Control(c) => Some(Incoming::Control(c)),
                        Message::Pair { kind, body } => Some(Incoming::Pair(kind, body.to_vec())),
                        Message::Cursor(_) => None,
                    };
                    self.buf.copy_within(n..self.len, 0);
                    self.len -= n;
                    got
                }
                Err(ControlError::Incomplete) => {
                    if !self.read_more(s)? {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    None
                }
                Err(ControlError::Malformed) => return Err(invalid("bad control message")),
            };
            if let Some(g) = got {
                return Ok(g);
            }
        }
    }
}

fn pack_v4(a: SocketAddrV4) -> u64 {
    ((u32::from(*a.ip()) as u64) << 16) | a.port() as u64
}

fn unpack_v4(v: u64) -> Option<SocketAddrV4> {
    (v != 0).then(|| SocketAddrV4::new(Ipv4Addr::from((v >> 16) as u32), v as u16))
}

fn new_session_id() -> u32 {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 4];
    if ring::rand::SystemRandom::new().fill(&mut b).is_err() {
        b = (clock::now_us() as u32).to_be_bytes();
    }
    u32::from_be_bytes(b).max(1)
}

// ---------------------------------------------------------------------------
// Host

/// What happened on the host that its user should see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// A device asked to pair: show this PIN. `new` = not shown before.
    PairingPin {
        device: String,
        pin: String,
        new: bool,
        addr: SocketAddr,
    },
    /// Pairing succeeded: `device` may now view and control this machine.
    Paired { device: String },
    /// A wrong (or expired) PIN; `left` more tries for the current PIN.
    WrongPin { device: String, left: u32 },
    /// Too many wrong PINs: pairing is off until the host restarts.
    PairingLocked,
    /// A device that isn't paired (or was forgotten) tried to connect.
    Refused {
        fingerprint: String,
        addr: SocketAddr,
    },
    /// The user said no to a paired device ("ask before each connection").
    Denied { device: String },
}

/// "Ask before each connection": true = allow this paired viewer.
pub type AskFn = Box<dyn FnMut(&Peer) -> bool + Send>;

/// Host behaviour.
pub struct HostOptions {
    /// Fixed pairing PIN (test tools only). `None`: a random PIN per request.
    pub fixed_pin: Option<String>,
    /// Asked before each connection of a paired viewer; `None` = always allow.
    pub ask: Option<AskFn>,
    pub events: Box<dyn FnMut(HostEvent) + Send>,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            fixed_pin: None,
            ask: None,
            events: Box::new(|_| {}),
        }
    }
}

/// Listens for viewers: TCP control and UDP media on the same port number.
pub struct HostListener {
    tcp: TcpListener,
    udp: UdpSocket,
    device: Arc<Device>,
    tls: Arc<rustls::ServerConfig>,
    opts: HostOptions,
    pins: PinState,
}

/// Host-side counters for one session.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct HostCounters {
    pub frames_sent: u64,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub keyframe_requests: u32,
    pub input: InputCounters,
    /// UDP datagrams that failed authentication or were replays.
    pub rejected_datagrams: u64,
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
    rejected: AtomicU64,
}

/// One connected viewer. Dropping it ends the session (and releases any
/// keys/buttons the viewer was holding).
pub struct HostSession {
    shared: Arc<HostShared>,
    udp: UdpSocket,
    out: TlsWriter,
    packetiser: Packetiser,
    sealer: Sealer,
    wire: [u8; WIRE_MAX],
    counters: HostCounters,
    cursor_buf: Vec<u8>,
    pub peer_tcp: SocketAddr,
    /// The paired viewer's name.
    pub peer_name: String,
    threads: Vec<JoinHandle<()>>,
}

impl HostListener {
    /// Binds `0.0.0.0:port` for TCP and UDP. `port = 0` picks a free one.
    pub fn bind(port: u16, device: Arc<Device>, opts: HostOptions) -> io::Result<Self> {
        Self::bind_on(Ipv4Addr::UNSPECIFIED, port, device, opts)
    }

    /// Like [`Self::bind`] on one interface (e.g. loopback-only for tests).
    pub fn bind_on(
        ip: Ipv4Addr,
        port: u16,
        device: Arc<Device>,
        opts: HostOptions,
    ) -> io::Result<Self> {
        // With port 0 the OS picks a free TCP port; its number may be taken
        // for UDP, so try a few.
        let mut tries = if port == 0 { 20 } else { 1 };
        let (tcp, udp) = loop {
            let tcp = TcpListener::bind((ip, port))?;
            let p = tcp.local_addr()?.port();
            match udp_socket(SocketAddrV4::new(ip, p)) {
                Ok(udp) => break (tcp, udp),
                Err(e) if tries <= 1 => return Err(e),
                Err(_) => tries -= 1,
            }
        };
        let tls = tls::server_config(&device.identity)?;
        let pins = PinState::new(opts.fixed_pin.clone());
        Ok(Self {
            tcp,
            udp,
            device,
            tls,
            opts,
            pins,
        })
    }

    pub fn port(&self) -> u16 {
        self.tcp.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn device(&self) -> &Arc<Device> {
        &self.device
    }

    /// True once too many wrong PINs have switched pairing off.
    pub fn pairing_locked(&self) -> bool {
        self.pins.locked()
    }

    fn event(&mut self, e: HostEvent) {
        (self.opts.events)(e)
    }

    /// Waits for one connection and handles it. A paired, allowed viewer gets
    /// a session (`Some`); a pairing request or a refused device is handled
    /// here and reported through `HostOptions::events` (`None`). An error is
    /// a failed connection attempt: call again to keep listening. Input
    /// actions go to `on_input` (on the UDP thread).
    pub fn accept(
        &mut self,
        info: StreamInfo,
        on_input: InputSink,
    ) -> io::Result<Option<HostSession>> {
        let (tcp, peer_tcp) = self.tcp.accept()?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let mut ch = tls::handshake(tcp, tls::server_conn(&self.tls)?)?;
        let mut reader = ControlReader::new();
        let version = match reader.read_one(&mut ch.reader)? {
            Incoming::Pair(PAIR_START, name) => {
                self.pair(&mut ch, &mut reader, &name, peer_tcp)?;
                return Ok(None);
            }
            Incoming::Control(Control::Hello { version, .. }) => version,
            // A viewer that doesn't know us looked and left.
            Incoming::Control(Control::Bye) => return Ok(None),
            _ => return Err(invalid("expected Hello")),
        };
        if version != PROTOCOL_VERSION {
            let _ = send_control(
                &ch.writer,
                Control::Reject {
                    reason: REJECT_VERSION,
                },
            );
            return Err(invalid("viewer speaks a different protocol version"));
        }
        let Some(peer) = self.device.peer_with(&ch.peer, ROLE_VIEWER) else {
            let _ = send_control(
                &ch.writer,
                Control::Reject {
                    reason: REJECT_NOT_PAIRED,
                },
            );
            self.event(HostEvent::Refused {
                fingerprint: fingerprint_short(&ch.peer),
                addr: peer_tcp,
            });
            return Ok(None);
        };
        if let Some(ask) = &mut self.opts.ask {
            if !ask(&peer) {
                let _ = send_control(
                    &ch.writer,
                    Control::Reject {
                        reason: REJECT_DENIED,
                    },
                );
                self.event(HostEvent::Denied { device: peer.name });
                return Ok(None);
            }
        }
        let session_id = new_session_id();
        send_control(
            &ch.writer,
            Control::Welcome {
                version: PROTOCOL_VERSION,
                info,
                udp_port: self.udp.local_addr()?.port(),
                session_id,
            },
        )?;
        let (to_viewer, from_viewer) = media_secrets(&ch.writer, session_id)?;

        let shared = Arc::new(HostShared {
            stop: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            keyframe: AtomicBool::new(true),
            keyframe_requests: AtomicU32::new(0),
            peer: AtomicU64::new(0),
            input_seq: AtomicU32::new(0),
            input: Mutex::new(InputCounters::default()),
            rejected: AtomicU64::new(0),
        });
        let mut threads = Vec::new();

        // Control: answer pings, take keyframe requests, notice disconnects.
        let Channel {
            reader: mut tls_in,
            writer: out,
            ..
        } = ch;
        tls_in.set_read_timeout(Some(Duration::from_millis(100)))?;
        let sh = shared.clone();
        let w = out.clone();
        threads.push(std::thread::spawn(move || {
            let mut reader = reader;
            while !sh.stop.load(Ordering::Relaxed) {
                let mut pong = None;
                let r = reader.read(&mut tls_in, |m| {
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
                    if send_control(&w, p).is_err() {
                        break;
                    }
                }
            }
            let _ = send_control(&w, Control::Bye);
            sh.connected.store(false, Ordering::Relaxed);
        }));

        // UDP from the viewer: punches (where to send video) and input. Only
        // datagrams sealed with this session's viewer key get past `open`.
        let udp = self.udp.try_clone()?;
        udp.set_read_timeout(Some(Duration::from_millis(100)))?;
        let client_ip = peer_tcp.ip();
        let sh = shared.clone();
        let mut on_input = on_input;
        threads.push(std::thread::spawn(move || {
            let mut buf = [0u8; WIRE_MAX + 64];
            let mut opener = Opener::new(from_viewer, session_id);
            let mut applier = InputApplier::new();
            while !sh.stop.load(Ordering::Relaxed) && sh.connected.load(Ordering::Relaxed) {
                let Ok((n, from)) = udp.recv_from(&mut buf) else {
                    continue;
                };
                let (SocketAddr::V4(v4), true) = (from, from.ip() == client_ip) else {
                    continue;
                };
                let Ok(d) = opener.open(&mut buf[..n]) else {
                    let s = opener.stats;
                    sh.rejected.store(s.bad + s.replayed, Ordering::Relaxed);
                    continue;
                };
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

        Ok(Some(HostSession {
            shared,
            udp: self.udp.try_clone()?,
            out,
            packetiser: Packetiser::default(),
            sealer: Sealer::new(to_viewer, session_id),
            wire: [0; WIRE_MAX],
            counters: HostCounters::default(),
            cursor_buf: Vec::with_capacity(FRAME_MAX),
            peer_tcp,
            peer_name: peer.name,
            threads,
        }))
    }

    /// Host side of PIN pairing (after the viewer's `PAIR_START`).
    fn pair(
        &mut self,
        ch: &mut Channel,
        reader: &mut ControlReader,
        name: &[u8],
        addr: SocketAddr,
    ) -> io::Result<()> {
        let device = clean_name(&String::from_utf8_lossy(name));
        let reject = |ch: &Channel, reason| {
            let _ = send_control(&ch.writer, Control::Reject { reason });
        };
        if self.pins.locked() {
            reject(ch, REJECT_PAIRING_LOCKED);
            self.event(HostEvent::PairingLocked);
            return Ok(());
        }
        let now = clock::now_us();
        let (pin, new) = self.pins.for_request(now);
        self.event(HostEvent::PairingPin {
            device: device.clone(),
            pin,
            new,
            addr,
        });
        send_pair(&ch.writer, PAIR_READY, self.device.name.as_bytes())?;
        // The viewer's user is typing the PIN.
        let wait = self.pins.remaining_us(now).max(1_000_000);
        ch.reader
            .set_read_timeout(Some(Duration::from_micros(wait)))?;
        let theirs = match reader.read_one(&mut ch.reader)? {
            Incoming::Pair(PAIR_PAKE, m) => m,
            Incoming::Control(Control::Bye) => return Ok(()), // cancelled
            _ => return Err(invalid("expected a pairing message")),
        };
        ch.reader.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        // Counted before anything is computed or sent.
        let Some(pin) = self.pins.begin_attempt(clock::now_us()) else {
            reject(ch, REJECT_WRONG_PIN);
            self.event(HostEvent::WrongPin { device, left: 0 });
            return Ok(());
        };
        let (pake, ours) = pairing::start(true, &pin, &ch.peer, &self.device.fingerprint());
        send_pair(&ch.writer, PAIR_PAKE, &ours)?;
        let mut binding = [0u8; 32];
        ch.writer.export(pairing::EXPORT_LABEL, &[], &mut binding)?;
        let confirm = pake.finish(&theirs, &binding);
        let mac = match reader.read_one(&mut ch.reader)? {
            Incoming::Pair(PAIR_CONFIRM, m) => m,
            _ => return Err(invalid("expected a pairing message")),
        };
        let Some(c) = confirm.filter(|c| c.verify(false, &mac)) else {
            let locked = self.pins.locked();
            reject(
                ch,
                if locked {
                    REJECT_PAIRING_LOCKED
                } else {
                    REJECT_WRONG_PIN
                },
            );
            let left = self.pins.left_for_pin();
            self.event(HostEvent::WrongPin { device, left });
            if locked {
                self.event(HostEvent::PairingLocked);
            }
            return Ok(());
        };
        self.pins.succeeded();
        self.device.add_peer(ch.peer, &device, ROLE_VIEWER)?;
        send_pair(&ch.writer, PAIR_CONFIRM, &c.mac(true))?;
        self.event(HostEvent::Paired { device });
        // Let the viewer read our confirmation before we close.
        let _ = reader.read_one(&mut ch.reader);
        Ok(())
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

    /// Seals and sends one encoded frame. Returns `Ok(false)` (and sends
    /// nothing) while the viewer's UDP address is still unknown.
    pub fn send_frame(&mut self, f: &OutFrame, data: &[u8]) -> io::Result<bool> {
        let Some(peer) = unpack_v4(self.shared.peer.load(Ordering::Relaxed)) else {
            return Ok(false);
        };
        let udp = &self.udp;
        let c = &mut self.counters;
        let sealer = &mut self.sealer;
        let wire = &mut self.wire;
        let n = self.packetiser.send_frame(f, data, |pkt| {
            let len = sealer.seal(pkt, wire);
            c.bytes_sent += len as u64;
            udp.send_to(&wire[..len], peer).map(|_| ())
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
        self.out.write_all(&self.cursor_buf)
    }

    /// Tells the viewer about a condition (`protocol::NOTICE_*`; `NOTICE_NONE` clears it).
    pub fn send_notice(&mut self, code: u8) -> io::Result<()> {
        send_control(&self.out, Control::HostNotice { code })
    }

    pub fn send_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
        send_control(&self.out, Control::CursorVisible { visible })
    }

    pub fn counters(&self) -> HostCounters {
        HostCounters {
            keyframe_requests: self.shared.keyframe_requests.load(Ordering::Relaxed),
            input: *lock(&self.shared.input),
            rejected_datagrams: self.shared.rejected.load(Ordering::Relaxed),
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
    /// We haven't paired with this host (or it has forgotten us): pair first.
    NotPaired = 6,
    /// The host refused (its user said no, or it's busy).
    Refused = 7,
    /// Pairing: wrong or expired PIN.
    WrongPin = 8,
    /// Pairing: too many wrong PINs; restart the host to pair.
    PairingLocked = 9,
    /// This device's identity couldn't be loaded, created or saved.
    Identity = 10,
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
            ConnectError::NotPaired => "not paired with this host (pair with its PIN first)",
            ConnectError::Refused => "the host refused the connection",
            ConnectError::WrongPin => "wrong or expired PIN",
            ConnectError::PairingLocked => {
                "too many wrong PINs: restart Glass on the host to pair again"
            }
            ConnectError::Identity => "could not load or save this device's identity",
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
    /// UDP datagrams that failed authentication / were replays.
    pub crypto_bad: u64,
    pub crypto_replayed: u64,
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

/// Input side of the client: builds datagrams, seals them and sends them on
/// the media socket. Shared by the UI thread (events), the control thread
/// (heartbeats) and the receiver (punches).
struct InputTx {
    sender: InputSender,
    udp: UdpSocket,
    dest: SocketAddrV4,
    sealer: Sealer,
    punch: [u8; PUNCH_SIZE],
    plain: [u8; MAX_INPUT_DATAGRAM],
    wire: [u8; WIRE_MAX],
    packets: u64,
    active: bool, // any input yet (no heartbeats before the first event)
}

impl InputTx {
    fn flush(&mut self, now: u64) {
        let (n, _) = self.sender.build(now, &mut self.plain);
        let len = self.sealer.seal(&self.plain[..n], &mut self.wire);
        let _ = self.udp.send_to(&self.wire[..len], self.dest);
        self.packets += 1;
        self.active = true;
    }

    fn send_punch(&mut self) {
        let len = self.sealer.seal(&self.punch, &mut self.wire);
        let _ = self.udp.send_to(&self.wire[..len], self.dest);
    }
}

struct ClientShared {
    stop: AtomicBool,
    connected: AtomicBool,
    want_keyframe: AtomicBool,
    keyframe_requests: AtomicU32,
    queue_full: AtomicU64,
    rx: AtomicRx,
    crypto_bad: AtomicU64,
    crypto_replayed: AtomicU64,
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
    peer: Peer,
    threads: Vec<JoinHandle<()>>,
}

/// Result of [`Client::wait_frame`].
pub enum Wait<'a> {
    Frame(LiveFrameMeta, &'a [u8]),
    Timeout,
    Disconnected,
}

fn reject_error(reason: u8) -> ConnectError {
    match reason {
        REJECT_VERSION => ConnectError::Version,
        REJECT_NOT_PAIRED => ConnectError::NotPaired,
        REJECT_PAIRING_LOCKED => ConnectError::PairingLocked,
        REJECT_WRONG_PIN => ConnectError::WrongPin,
        _ => ConnectError::Refused, // REJECT_DENIED, REJECT_BUSY
    }
}

/// TCP connect + TLS handshake (no pinning decision yet).
fn open_channel(
    device: &Device,
    host: &str,
    port: u16,
) -> Result<(Channel, SocketAddrV4), ConnectError> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|_| ConnectError::Resolve)?
        .find_map(|a| match a {
            SocketAddr::V4(v4) => Some(v4),
            SocketAddr::V6(_) => None,
        })
        .ok_or(ConnectError::Resolve)?;
    let tcp = TcpStream::connect_timeout(&SocketAddr::V4(addr), Duration::from_secs(3))
        .map_err(|_| ConnectError::Connect)?;
    let hs = |_| ConnectError::Handshake;
    tcp.set_nodelay(true).map_err(hs)?;
    tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).map_err(hs)?;
    let cfg = tls::client_config(&device.identity).map_err(|_| ConnectError::Identity)?;
    let conn = tls::client_conn(&cfg).map_err(hs)?;
    let ch = tls::handshake(tcp, conn).map_err(hs)?;
    Ok((ch, addr))
}

/// A pairing in progress: the host is showing a PIN.
pub struct Pairing {
    ch: Channel,
    reader: ControlReader,
    device: Arc<Device>,
    host_name: String,
}

/// Asks `host` to pair. On success the host is showing a PIN; get it from
/// the user and call [`Pairing::finish`] (dropping the `Pairing` cancels).
pub fn pair_begin(device: &Arc<Device>, host: &str, port: u16) -> Result<Pairing, ConnectError> {
    let (mut ch, _) = open_channel(device, host, port)?;
    let hs = |_| ConnectError::Handshake;
    send_pair(&ch.writer, PAIR_START, device.name.as_bytes()).map_err(hs)?;
    let mut reader = ControlReader::new();
    match reader.read_one(&mut ch.reader).map_err(hs)? {
        Incoming::Pair(PAIR_READY, name) => Ok(Pairing {
            ch,
            reader,
            device: device.clone(),
            host_name: clean_name(&String::from_utf8_lossy(&name)),
        }),
        Incoming::Control(Control::Reject { reason }) => Err(reject_error(reason)),
        _ => Err(ConnectError::Handshake),
    }
}

impl Pairing {
    /// The name the host gave (shown to the user next to the PIN prompt).
    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    pub fn host_fingerprint(&self) -> Fingerprint {
        self.ch.peer
    }

    /// Runs the PIN exchange. On success both sides have stored each other.
    pub fn finish(mut self, pin: &str) -> Result<Peer, ConnectError> {
        let hs = |_| ConnectError::Handshake;
        let ch = &mut self.ch;
        let (pake, ours) = pairing::start(false, pin, &self.device.fingerprint(), &ch.peer);
        send_pair(&ch.writer, PAIR_PAKE, &ours).map_err(hs)?;
        ch.reader
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(hs)?;
        let theirs = match self.reader.read_one(&mut ch.reader).map_err(hs)? {
            Incoming::Pair(PAIR_PAKE, m) => m,
            Incoming::Control(Control::Reject { reason }) => return Err(reject_error(reason)),
            _ => return Err(ConnectError::Handshake),
        };
        let mut binding = [0u8; 32];
        ch.writer
            .export(pairing::EXPORT_LABEL, &[], &mut binding)
            .map_err(hs)?;
        let c = pake
            .finish(&theirs, &binding)
            .ok_or(ConnectError::Handshake)?;
        send_pair(&ch.writer, PAIR_CONFIRM, &c.mac(false)).map_err(hs)?;
        match self.reader.read_one(&mut ch.reader).map_err(hs)? {
            Incoming::Pair(PAIR_CONFIRM, m) if c.verify(true, &m) => {}
            Incoming::Control(Control::Reject { reason }) => return Err(reject_error(reason)),
            _ => return Err(ConnectError::Handshake),
        }
        self.device
            .add_peer(ch.peer, &self.host_name, ROLE_HOST)
            .map_err(|_| ConnectError::Identity)?;
        // (Drop says Bye.)
        self.device
            .peer_with(&ch.peer, ROLE_HOST)
            .ok_or(ConnectError::Identity)
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        let _ = send_control(&self.ch.writer, Control::Bye);
    }
}

impl Client {
    /// Connects to a paired host at `host:port`, completes the handshake and
    /// starts receiving. A host we haven't paired with gives
    /// `ConnectError::NotPaired` before anything about us is sent.
    pub fn connect(device: &Arc<Device>, host: &str, port: u16) -> Result<Self, ConnectError> {
        let (mut ch, addr) = open_channel(device, host, port)?;
        let hs = |_| ConnectError::Handshake;
        let Some(peer) = device.peer_with(&ch.peer, ROLE_HOST) else {
            let _ = send_control(&ch.writer, Control::Bye);
            return Err(ConnectError::NotPaired);
        };
        send_control(
            &ch.writer,
            Control::Hello {
                version: PROTOCOL_VERSION,
                display_w: 0, // LATER(M7): request native resolution.
                display_h: 0,
            },
        )
        .map_err(hs)?;
        ch.reader
            .set_read_timeout(Some(APPROVAL_TIMEOUT))
            .map_err(hs)?;
        let mut reader = ControlReader::new();
        let (info, udp_port, session_id) = match reader.read_one(&mut ch.reader).map_err(hs)? {
            Incoming::Control(Control::Welcome {
                version,
                info,
                udp_port,
                session_id,
            }) if version == PROTOCOL_VERSION => (info, udp_port, session_id),
            Incoming::Control(Control::Welcome { .. }) => return Err(ConnectError::Version),
            Incoming::Control(Control::Reject { reason }) => return Err(reject_error(reason)),
            _ => return Err(ConnectError::Handshake),
        };
        let (from_host, to_host) = media_secrets(&ch.writer, session_id).map_err(hs)?;
        let host_udp = SocketAddrV4::new(*addr.ip(), udp_port);
        let sock = |_| ConnectError::Socket;
        let udp = udp_socket(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).map_err(sock)?;
        udp.set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(sock)?;
        let mut tx = InputTx {
            sender: InputSender::new(session_id),
            udp: udp.try_clone().map_err(sock)?,
            dest: host_udp,
            sealer: Sealer::new(to_host, session_id),
            punch: encode_punch(session_id),
            plain: [0; MAX_INPUT_DATAGRAM],
            wire: [0; WIRE_MAX],
            packets: 0,
            active: false,
        };
        tx.send_punch();
        ch.reader
            .set_read_timeout(Some(Duration::from_millis(20)))
            .map_err(hs)?;

        let shared = Arc::new(ClientShared {
            stop: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            want_keyframe: AtomicBool::new(false),
            keyframe_requests: AtomicU32::new(0),
            queue_full: AtomicU64::new(0),
            rx: AtomicRx::default(),
            crypto_bad: AtomicU64::new(0),
            crypto_replayed: AtomicU64::new(0),
            clock_valid: AtomicBool::new(false),
            clock_offset: AtomicI64::new(0),
            clock_rtt: AtomicU32::new(0),
            last_rtt: AtomicU32::new(0),
            host_notice: std::sync::atomic::AtomicU8::new(0),
            queue: FrameQueue::new(),
            input: Mutex::new(tx),
            cursor: Mutex::new(CursorState::default()),
        });
        let Channel {
            reader: tls_in,
            writer,
            ..
        } = ch;
        let threads = vec![
            spawn_receiver(
                shared.clone(),
                udp,
                host_udp,
                Opener::new(from_host, session_id),
            ),
            spawn_control(shared.clone(), tls_in, writer, reader),
        ];
        Ok(Self {
            shared,
            info,
            host: addr,
            peer,
            threads,
        })
    }

    pub fn info(&self) -> StreamInfo {
        self.info
    }

    pub fn host(&self) -> SocketAddrV4 {
        self.host
    }

    /// The paired host we're connected to.
    pub fn peer(&self) -> &Peer {
        &self.peer
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
            crypto_bad: sh.crypto_bad.load(Ordering::Relaxed),
            crypto_replayed: sh.crypto_replayed.load(Ordering::Relaxed),
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

/// Datagrams -> open -> reassembler -> queue. Re-punches every second until
/// video flows, then every 5 s.
fn spawn_receiver(
    sh: Arc<ClientShared>,
    udp: UdpSocket,
    host_udp: SocketAddrV4,
    mut opener: Opener,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut r = Reassembler::new();
        let mut buf = [0u8; WIRE_MAX + 64];
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
                lock(&sh.input).send_punch();
                last_punch = now;
            }
            let Ok((n, from)) = udp.recv_from(&mut buf) else {
                continue;
            };
            if from.ip() != host_ip {
                continue;
            }
            let plain = match opener.open(&mut buf[..n]) {
                Ok(p) => p,
                Err(_) => {
                    sh.crypto_bad.store(opener.stats.bad, Ordering::Relaxed);
                    sh.crypto_replayed
                        .store(opener.stats.replayed, Ordering::Relaxed);
                    continue;
                }
            };
            if let Some(f) = r.push(plain, clock::now_us()) {
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
    mut tls_in: TlsReader,
    out: TlsWriter,
    mut reader: ControlReader,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut sync = ClockSync::default();
        let mut seq = 0u32;
        let mut last_ping = 0u64;
        let mut last_kf = 0u64;
        loop {
            if sh.stop.load(Ordering::Relaxed) {
                let _ = send_control(&out, Control::Bye);
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
                ok &= send_control(&out, ping).is_ok();
            }
            if now.saturating_sub(last_kf) >= KEYFRAME_RETRY_US
                && sh.want_keyframe.swap(false, Ordering::Relaxed)
            {
                last_kf = now;
                sh.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                ok &= send_control(&out, Control::KeyframeRequest).is_ok();
            }
            {
                let mut tx = lock(&sh.input);
                if tx.active && tx.sender.heartbeat_due(now) {
                    tx.flush(now);
                }
            }
            let r = reader.read(&mut tls_in, |m| {
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
