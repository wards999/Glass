//! Input back-channel (viewer -> host over UDP), loss-tolerant by design.
//!
//! Each datagram carries
//! - **recent events** (key down/up, button down/up, scroll), each with a
//!   sequence number and repeated in every packet for [`EVENT_WINDOW_US`], so a
//!   lost datagram doesn't lose a keystroke or click; the host applies each
//!   event once, in order;
//! - **current state** (pointer position, buttons held, keys held), which the
//!   host reconciles against what it holds, so nothing stays stuck down even if
//!   every copy of an event was lost.
//!
//! The viewer sends on every change and a heartbeat every [`HEARTBEAT_US`].
//! Pointer position is normalised to the video picture: 0..=65535 on each axis.

use crate::keymap::is_modifier;

pub const INPUT_MAGIC: [u8; 4] = *b"GLIN";
pub const MAX_EVENTS: usize = 16;
pub const MAX_KEYS: usize = 16;
/// Events are repeated in packets for this long after they happen.
pub const EVENT_WINDOW_US: u64 = 300_000;
pub const HEARTBEAT_US: u64 = 50_000;
const HEADER: usize = 24;
const EVENT_SIZE: usize = 8;
pub const MAX_INPUT_DATAGRAM: usize = HEADER + MAX_KEYS + MAX_EVENTS * EVENT_SIZE;

/// Mouse buttons (bit index in the button mask).
pub const BUTTON_LEFT: u8 = 0;
pub const BUTTON_RIGHT: u8 = 1;
pub const BUTTON_MIDDLE: u8 = 2;
pub const BUTTON_BACK: u8 = 3;
pub const BUTTON_FORWARD: u8 = 4;
const BUTTONS: u8 = 5;

/// Windows wheel units: 120 = one notch (three lines by default).
pub const WHEEL_DELTA: i16 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Key {
        hid: u8,
        down: bool,
    },
    /// Position where the button changed (normalised).
    Button {
        button: u8,
        down: bool,
        x: u16,
        y: u16,
    },
    /// Wheel units; `dy > 0` scrolls up (content moves down), `dx > 0` right.
    Scroll {
        dy: i16,
        dx: i16,
    },
}

impl InputEvent {
    fn encode(&self, b: &mut [u8]) {
        let (kind, code, flag, value) = match *self {
            InputEvent::Key { hid, down } => (1, hid, down as u8, 0u32),
            InputEvent::Button { button, down, x, y } => {
                (2, button, down as u8, (x as u32) << 16 | y as u32)
            }
            InputEvent::Scroll { dy, dx } => (3, 0, 0, (dy as u16 as u32) << 16 | dx as u16 as u32),
        };
        b[0] = kind;
        b[1] = code;
        b[2] = flag;
        b[3] = 0;
        b[4..8].copy_from_slice(&value.to_be_bytes());
    }

    fn decode(b: &[u8]) -> Option<Self> {
        let v = u32::from_be_bytes(b[4..8].try_into().ok()?);
        Some(match b[0] {
            1 => InputEvent::Key {
                hid: b[1],
                down: b[2] != 0,
            },
            2 if b[1] < BUTTONS => InputEvent::Button {
                button: b[1],
                down: b[2] != 0,
                x: (v >> 16) as u16,
                y: v as u16,
            },
            3 => InputEvent::Scroll {
                dy: (v >> 16) as u16 as i16,
                dx: v as u16 as i16,
            },
            _ => return None,
        })
    }
}

/// One input datagram, decoded. Fixed-size arrays: no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPacket {
    pub session_id: u32,
    pub packet_seq: u32,
    /// Sequence number of `events[0]`; the rest follow consecutively.
    pub first_event_seq: u32,
    pub event_count: u8,
    pub events: [InputEvent; MAX_EVENTS],
    pub buttons: u8,
    pub pos: Option<(u16, u16)>,
    pub key_count: u8,
    pub keys: [u8; MAX_KEYS],
}

impl Default for InputPacket {
    fn default() -> Self {
        Self {
            session_id: 0,
            packet_seq: 0,
            first_event_seq: 0,
            event_count: 0,
            events: [InputEvent::Scroll { dy: 0, dx: 0 }; MAX_EVENTS],
            buttons: 0,
            pos: None,
            key_count: 0,
            keys: [0; MAX_KEYS],
        }
    }
}

impl InputPacket {
    /// Encodes into `buf`; returns the datagram length.
    pub fn encode(&self, buf: &mut [u8; MAX_INPUT_DATAGRAM]) -> usize {
        buf[0..4].copy_from_slice(&INPUT_MAGIC);
        buf[4..8].copy_from_slice(&self.session_id.to_be_bytes());
        buf[8..12].copy_from_slice(&self.packet_seq.to_be_bytes());
        buf[12..16].copy_from_slice(&self.first_event_seq.to_be_bytes());
        buf[16] = self.event_count;
        buf[17] = self.key_count;
        buf[18] = self.buttons;
        buf[19] = self.pos.is_some() as u8;
        let (x, y) = self.pos.unwrap_or((0, 0));
        buf[20..22].copy_from_slice(&x.to_be_bytes());
        buf[22..24].copy_from_slice(&y.to_be_bytes());
        let mut n = HEADER;
        let kc = self.key_count as usize;
        buf[n..n + kc].copy_from_slice(&self.keys[..kc]);
        n += kc;
        for e in &self.events[..self.event_count as usize] {
            e.encode(&mut buf[n..n + EVENT_SIZE]);
            n += EVENT_SIZE;
        }
        n
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < HEADER || b[0..4] != INPUT_MAGIC {
            return None;
        }
        let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
        let u16_at = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        let (ec, kc) = (b[16] as usize, b[17] as usize);
        if ec > MAX_EVENTS || kc > MAX_KEYS || b.len() != HEADER + kc + ec * EVENT_SIZE {
            return None;
        }
        let mut p = InputPacket {
            session_id: u32_at(4),
            packet_seq: u32_at(8),
            first_event_seq: u32_at(12),
            event_count: ec as u8,
            buttons: b[18],
            pos: (b[19] & 1 != 0).then(|| (u16_at(20), u16_at(22))),
            key_count: kc as u8,
            ..Default::default()
        };
        p.keys[..kc].copy_from_slice(&b[HEADER..HEADER + kc]);
        let ev = &b[HEADER + kc..];
        for i in 0..ec {
            p.events[i] = InputEvent::decode(&ev[i * EVENT_SIZE..(i + 1) * EVENT_SIZE])?;
        }
        Some(p)
    }
}

/// Wrapping sequence order: true if `a` comes after `b`.
fn after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

// ---------------------------------------------------------------------------
// Viewer side

/// Builds input datagrams from UI events. Remembers recent events (for
/// repetition), held keys/buttons and the pointer, plus when each packet was
/// sent (for input -> screen latency).
pub struct InputSender {
    session_id: u32,
    packet_seq: u32,
    next_event_seq: u32,
    recent: [(u64, InputEvent); MAX_EVENTS], // ring: (time, event), seq order
    recent_len: usize,
    recent_head: usize, // index of the oldest
    keys: [bool; 256],
    buttons: u8,
    pos: Option<(u16, u16)>,
    sent: [(u32, u64); 1024], // packet_seq -> send time
    last_send_us: u64,
    pub events_total: u64,
}

impl InputSender {
    pub fn new(session_id: u32) -> Self {
        Self {
            session_id,
            packet_seq: 0,
            next_event_seq: 1,
            recent: [(0, InputEvent::Scroll { dy: 0, dx: 0 }); MAX_EVENTS],
            recent_len: 0,
            recent_head: 0,
            keys: [false; 256],
            buttons: 0,
            pos: None,
            sent: [(0, 0); 1024],
            last_send_us: 0,
            events_total: 0,
        }
    }

    fn push_event(&mut self, now: u64, e: InputEvent) {
        if self.recent_len == MAX_EVENTS {
            self.recent_head = (self.recent_head + 1) % MAX_EVENTS;
            self.recent_len -= 1;
        }
        let tail = (self.recent_head + self.recent_len) % MAX_EVENTS;
        self.recent[tail] = (now, e);
        self.recent_len += 1;
        self.next_event_seq = self.next_event_seq.wrapping_add(1);
        self.events_total += 1;
    }

    /// Key down (also for auto-repeat) or up.
    pub fn key(&mut self, now: u64, hid: u8, down: bool) {
        if hid == 0 || (!down && !self.keys[hid as usize]) {
            return;
        }
        self.keys[hid as usize] = down;
        self.push_event(now, InputEvent::Key { hid, down });
    }

    pub fn button(&mut self, now: u64, button: u8, down: bool, x: u16, y: u16) {
        if button >= BUTTONS {
            return;
        }
        let bit = 1 << button;
        if down == (self.buttons & bit != 0) {
            return;
        }
        self.buttons ^= bit;
        self.pos = Some((x, y));
        self.push_event(now, InputEvent::Button { button, down, x, y });
    }

    pub fn move_to(&mut self, x: u16, y: u16) {
        self.pos = Some((x, y));
    }

    pub fn scroll(&mut self, now: u64, dy: i16, dx: i16) {
        if dy != 0 || dx != 0 {
            self.push_event(now, InputEvent::Scroll { dy, dx });
        }
    }

    /// Lets go of every held key and button (focus lost, disconnect).
    pub fn release_all(&mut self, now: u64) {
        for hid in 0..=255u8 {
            if self.keys[hid as usize] && !is_modifier(hid) {
                self.key(now, hid, false);
            }
        }
        for hid in 0..=255u8 {
            if self.keys[hid as usize] {
                self.key(now, hid, false);
            }
        }
        for b in 0..BUTTONS {
            let (x, y) = self.pos.unwrap_or((0, 0));
            self.button(now, b, false, x, y);
        }
    }

    /// True if a heartbeat is due.
    pub fn heartbeat_due(&self, now: u64) -> bool {
        now.saturating_sub(self.last_send_us) >= HEARTBEAT_US
    }

    /// Builds the next datagram (state + events from the last
    /// [`EVENT_WINDOW_US`]). Returns its length and packet sequence number.
    pub fn build(&mut self, now: u64, buf: &mut [u8; MAX_INPUT_DATAGRAM]) -> (usize, u32) {
        self.packet_seq = self.packet_seq.wrapping_add(1);
        let mut p = InputPacket {
            session_id: self.session_id,
            packet_seq: self.packet_seq,
            buttons: self.buttons,
            pos: self.pos,
            ..Default::default()
        };
        // Recent events still inside the window (they are in time order).
        let mut skip = 0;
        while skip < self.recent_len {
            let (t, _) = self.recent[(self.recent_head + skip) % MAX_EVENTS];
            if now.saturating_sub(t) <= EVENT_WINDOW_US {
                break;
            }
            skip += 1;
        }
        let n = self.recent_len - skip;
        p.first_event_seq = self.next_event_seq.wrapping_sub(n as u32);
        for i in 0..n {
            p.events[i] = self.recent[(self.recent_head + skip + i) % MAX_EVENTS].1;
        }
        p.event_count = n as u8;
        let mut kc = 0;
        for hid in 0..=255u8 {
            if self.keys[hid as usize] && kc < MAX_KEYS {
                p.keys[kc] = hid;
                kc += 1;
            }
        }
        p.key_count = kc as u8;
        self.sent[self.packet_seq as usize % self.sent.len()] = (self.packet_seq, now);
        self.last_send_us = now;
        (p.encode(buf), self.packet_seq)
    }

    /// When packet `seq` was built, if still remembered.
    pub fn sent_at(&self, seq: u32) -> Option<u64> {
        let (s, t) = self.sent[seq as usize % self.sent.len()];
        (s == seq && seq != 0).then_some(t)
    }
}

// ---------------------------------------------------------------------------
// Host side

/// What the host should inject, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    MoveTo { x: u16, y: u16 },
    Button { button: u8, down: bool },
    Key { hid: u8, down: bool },
    Scroll { dy: i16, dx: i16 },
}

/// Host-side counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputCounters {
    pub packets: u64,
    pub keys: u64,
    pub clicks: u64,
    pub scrolls: u64,
    pub moves: u64,
    /// Keys/buttons changed by state reconciliation (their events were lost).
    pub repaired: u64,
}

/// Turns input datagrams into injection actions: each event once and in
/// order, then reconciles held keys/buttons with the packet's state.
pub struct InputApplier {
    last_packet: Option<u32>,
    last_event: Option<u32>,
    keys: [bool; 256],
    buttons: u8,
    pos: Option<(u16, u16)>,
    /// Latest packet that changed anything (for `FrameInfo::input_seq`).
    pub applied_seq: u32,
    pub counters: InputCounters,
}

impl Default for InputApplier {
    fn default() -> Self {
        Self::new()
    }
}

impl InputApplier {
    pub fn new() -> Self {
        Self {
            last_packet: None,
            last_event: None,
            keys: [false; 256],
            buttons: 0,
            pos: None,
            applied_seq: 0,
            counters: InputCounters::default(),
        }
    }

    fn move_to(&mut self, x: u16, y: u16, out: &mut impl FnMut(InputAction)) -> bool {
        if self.pos == Some((x, y)) {
            return false;
        }
        self.pos = Some((x, y));
        self.counters.moves += 1;
        out(InputAction::MoveTo { x, y });
        true
    }

    fn set_key(&mut self, hid: u8, down: bool, out: &mut impl FnMut(InputAction)) {
        self.keys[hid as usize] = down;
        out(InputAction::Key { hid, down });
    }

    fn set_button(&mut self, button: u8, down: bool, out: &mut impl FnMut(InputAction)) {
        if down {
            self.buttons |= 1 << button;
        } else {
            self.buttons &= !(1 << button);
        }
        out(InputAction::Button { button, down });
    }

    /// Applies one packet. Returns true if it changed anything.
    pub fn apply(&mut self, p: &InputPacket, out: &mut impl FnMut(InputAction)) -> bool {
        self.counters.packets += 1;
        let mut changed = false;
        for i in 0..p.event_count as usize {
            let seq = p.first_event_seq.wrapping_add(i as u32);
            if self.last_event.is_some_and(|l| !after(seq, l)) {
                continue; // already applied (repeated copy)
            }
            self.last_event = Some(seq);
            changed = true;
            match p.events[i] {
                InputEvent::Key { hid, down } => {
                    if down || self.keys[hid as usize] {
                        self.counters.keys += down as u64;
                        self.set_key(hid, down, out);
                    }
                }
                InputEvent::Button { button, down, x, y } => {
                    self.move_to(x, y, out);
                    if down != (self.buttons & (1 << button) != 0) {
                        self.counters.clicks += down as u64;
                        self.set_button(button, down, out);
                    }
                }
                InputEvent::Scroll { dy, dx } => {
                    self.counters.scrolls += 1;
                    out(InputAction::Scroll { dy, dx });
                }
            }
        }
        // State: only from packets newer than any seen.
        if self.last_packet.is_none_or(|l| after(p.packet_seq, l)) {
            self.last_packet = Some(p.packet_seq);
            if let Some((x, y)) = p.pos {
                changed |= self.move_to(x, y, out);
            }
            for b in 0..BUTTONS {
                let want = p.buttons & (1 << b) != 0;
                if want != (self.buttons & (1 << b) != 0) {
                    self.counters.repaired += 1;
                    self.set_button(b, want, out);
                    changed = true;
                }
            }
            let mut want = [false; 256];
            for &k in &p.keys[..p.key_count as usize] {
                want[k as usize] = true;
            }
            // Release non-modifiers first, then modifiers; press modifiers first.
            for pass_mod in [false, true] {
                for hid in 0..=255u8 {
                    if is_modifier(hid) == pass_mod
                        && self.keys[hid as usize]
                        && !want[hid as usize]
                    {
                        self.counters.repaired += 1;
                        self.set_key(hid, false, out);
                        changed = true;
                    }
                }
            }
            for pass_mod in [true, false] {
                for hid in 0..=255u8 {
                    if is_modifier(hid) == pass_mod
                        && want[hid as usize]
                        && !self.keys[hid as usize]
                    {
                        self.counters.repaired += 1;
                        self.set_key(hid, true, out);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.applied_seq = p.packet_seq;
        }
        changed
    }

    /// Releases everything held (session over).
    pub fn release_all(&mut self, out: &mut impl FnMut(InputAction)) {
        for pass_mod in [false, true] {
            for hid in 0..=255u8 {
                if is_modifier(hid) == pass_mod && self.keys[hid as usize] {
                    self.set_key(hid, false, out);
                }
            }
        }
        for b in 0..BUTTONS {
            if self.buttons & (1 << b) != 0 {
                self.set_button(b, false, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{HID_LCTRL, HID_LSHIFT};

    fn round_trip(s: &mut InputSender, now: u64) -> InputPacket {
        let mut buf = [0u8; MAX_INPUT_DATAGRAM];
        let (n, _) = s.build(now, &mut buf);
        InputPacket::decode(&buf[..n]).expect("decodes")
    }

    fn collect(a: &mut InputApplier, p: &InputPacket) -> Vec<InputAction> {
        let mut v = Vec::new();
        a.apply(p, &mut |x| v.push(x));
        v
    }

    #[test]
    fn packet_round_trip() {
        let mut s = InputSender::new(7);
        s.key(1, HID_LSHIFT, true);
        s.key(2, 0x04, true);
        s.button(3, BUTTON_RIGHT, true, 100, 65535);
        s.scroll(4, -240, 60);
        let p = round_trip(&mut s, 5);
        assert_eq!(p.session_id, 7);
        assert_eq!(p.event_count, 4);
        assert_eq!(p.first_event_seq, 1);
        assert_eq!(
            p.events[2],
            InputEvent::Button {
                button: BUTTON_RIGHT,
                down: true,
                x: 100,
                y: 65535
            }
        );
        assert_eq!(p.events[3], InputEvent::Scroll { dy: -240, dx: 60 });
        assert_eq!(&p.keys[..p.key_count as usize], &[0x04, HID_LSHIFT]);
        assert_eq!(p.buttons, 1 << BUTTON_RIGHT);
        assert_eq!(p.pos, Some((100, 65535)));
        assert!(InputPacket::decode(&[0; 10]).is_none());
        assert_eq!(s.sent_at(p.packet_seq), Some(5));
    }

    #[test]
    fn repeated_events_apply_once_in_order() {
        let mut s = InputSender::new(1);
        let mut a = InputApplier::new();
        s.key(0, HID_LCTRL, true);
        s.key(1, 0x06, true); // C
        let first = collect(&mut a, &round_trip(&mut s, 2));
        assert_eq!(
            first,
            vec![
                InputAction::Key {
                    hid: HID_LCTRL,
                    down: true
                },
                InputAction::Key {
                    hid: 0x06,
                    down: true
                }
            ]
        );
        s.key(3, 0x06, false);
        // The next packet repeats the first two events; only the new one applies.
        let second = collect(&mut a, &round_trip(&mut s, 4));
        assert_eq!(
            second,
            vec![InputAction::Key {
                hid: 0x06,
                down: false
            }]
        );
        // Heartbeat: nothing new.
        assert!(collect(&mut a, &round_trip(&mut s, 5)).is_empty());
        // Auto-repeat: a second down for a held key is injected again.
        s.key(6, 0x51, true);
        s.key(7, 0x51, true);
        let rep = collect(&mut a, &round_trip(&mut s, 8));
        assert_eq!(rep.len(), 2);
    }

    #[test]
    fn events_age_out_of_the_window() {
        let mut s = InputSender::new(1);
        s.key(0, 0x04, true);
        s.key(1, 0x04, false);
        assert_eq!(round_trip(&mut s, 1_000).event_count, 2);
        assert_eq!(round_trip(&mut s, EVENT_WINDOW_US + 10).event_count, 0);
    }

    #[test]
    fn click_carries_position() {
        let mut s = InputSender::new(1);
        let mut a = InputApplier::new();
        s.move_to(10, 10);
        s.button(1, BUTTON_LEFT, true, 500, 600);
        s.button(2, BUTTON_LEFT, false, 500, 600);
        let acts = collect(&mut a, &round_trip(&mut s, 3));
        assert_eq!(
            acts,
            vec![
                InputAction::MoveTo { x: 500, y: 600 },
                InputAction::Button {
                    button: BUTTON_LEFT,
                    down: true
                },
                InputAction::Button {
                    button: BUTTON_LEFT,
                    down: false
                },
            ]
        );
        assert_eq!(a.counters.clicks, 1);
    }

    #[test]
    fn state_repairs_lost_events() {
        // The host never sees the event packets, only a later heartbeat
        // whose window no longer contains them.
        let mut s = InputSender::new(1);
        let mut a = InputApplier::new();
        s.key(0, HID_LSHIFT, true);
        s.key(0, 0x04, true);
        let _lost = round_trip(&mut s, 1);
        let late = round_trip(&mut s, EVENT_WINDOW_US + 100);
        let acts = collect(&mut a, &late);
        // Modifier pressed first.
        assert_eq!(
            acts[0],
            InputAction::Key {
                hid: HID_LSHIFT,
                down: true
            }
        );
        assert_eq!(
            acts[1],
            InputAction::Key {
                hid: 0x04,
                down: true
            }
        );
        // Releases also repaired, non-modifier first.
        s.key(EVENT_WINDOW_US + 200, 0x04, false);
        s.key(EVENT_WINDOW_US + 200, HID_LSHIFT, false);
        let _lost = round_trip(&mut s, EVENT_WINDOW_US + 201);
        let acts = collect(&mut a, &round_trip(&mut s, 3 * EVENT_WINDOW_US));
        assert_eq!(
            acts,
            vec![
                InputAction::Key {
                    hid: 0x04,
                    down: false
                },
                InputAction::Key {
                    hid: HID_LSHIFT,
                    down: false
                }
            ]
        );
        assert_eq!(a.counters.repaired, 4);
    }

    #[test]
    fn old_packets_dont_roll_back_state() {
        let mut s = InputSender::new(1);
        let mut a = InputApplier::new();
        s.key(0, 0x04, true);
        let p1 = round_trip(&mut s, 1);
        s.key(2, 0x04, false);
        let p2 = round_trip(&mut s, 3);
        collect(&mut a, &p2); // arrives first: down then up
                              // Reordered older packet: its events are old and its state is stale.
        assert!(collect(&mut a, &p1).is_empty());
    }

    /// 20% random loss, 2000 random events: nothing ends up held, and almost
    /// every keystroke is delivered exactly once.
    #[test]
    fn survives_heavy_loss() {
        let mut rng = 0x1234_5678u64;
        let mut rand = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut s = InputSender::new(1);
        let mut a = InputApplier::new();
        let mut held = [false; 256];
        let (mut now, mut downs, mut delivered, mut lost) = (0u64, 0u64, 0u64, 0u64);
        let mut buf = [0u8; MAX_INPUT_DATAGRAM];
        for _ in 0..2000 {
            now += 5_000 + rand() % 20_000;
            let hid = [0x04u8, 0x05, 0x06, HID_LSHIFT, HID_LCTRL][(rand() % 5) as usize];
            let down = !held[hid as usize];
            held[hid as usize] = down;
            downs += down as u64;
            s.key(now, hid, down);
            let (n, _) = s.build(now, &mut buf);
            if rand() % 100 < 20 {
                lost += 1;
                continue;
            }
            let p = InputPacket::decode(&buf[..n]).unwrap();
            a.apply(&p, &mut |act| {
                if let InputAction::Key { down: true, .. } = act {
                    delivered += 1;
                }
            });
        }
        s.release_all(now + 1);
        // A few heartbeats get through at the end.
        for k in 0..3 {
            let (n, _) = s.build(now + 100_000 * (k + 1), &mut buf);
            a.apply(&InputPacket::decode(&buf[..n]).unwrap(), &mut |_| {});
        }
        assert!(lost > 300, "loss simulated: {lost}");
        assert!(a.keys.iter().all(|k| !k), "a key is stuck");
        assert_eq!(a.buttons, 0);
        let rate = delivered as f64 / downs as f64;
        assert!(
            (0.99..=1.0).contains(&rate),
            "delivered {delivered}/{downs}"
        );
    }
}
