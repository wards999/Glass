//! Media datagram encryption (UDP video, input, punches): AES-128-GCM with
//! per-direction keys from the TLS exporter (see `net.rs`).
//!
//! Wire format of every UDP datagram: `counter u64 BE | ciphertext | tag (16)`.
//! The counter starts at 1 and only goes up, so the nonce (`iv XOR counter`)
//! never repeats under a key. AAD = counter + session id.
//!
//! Keys roll forward every 2^23 packets (an "epoch"): epoch `e+1`'s secret is
//! HKDF-Expand(epoch `e`'s secret), and old secrets are dropped, so a key never
//! comes near AES-GCM's usage limits and a stolen key can't decrypt the past.
//! The receiver keeps a 1024-packet replay window.
//!
//! Hot path: no allocation. `seal` copies the plaintext once into the caller's
//! wire buffer and encrypts in place; `open` decrypts in place.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM};
use ring::hkdf;

const COUNTER: usize = 8;
const TAG: usize = 16;
/// Bytes added to each datagram: counter (8) + tag (16).
pub const OVERHEAD: usize = 8 + 16;
const _: () = assert!(OVERHEAD == COUNTER + TAG);
/// Largest datagram on the wire (fits a 1500-byte MTU with IP/UDP headers
/// and room to spare for tunnels).
pub const WIRE_MAX: usize = 1400;
/// Packets per key epoch. 2^23 ≈ 8.4 M is ~4.5x below the 2^24.5 full-size
/// records TLS 1.3 allows per AES-GCM key.
const EPOCH_BITS: u32 = 23;
/// A receiver follows the sender up to this many epochs ahead (packets lost
/// across a whole epoch boundary).
const MAX_EPOCH_JUMP: u64 = 16;
/// Replay window, packets.
const WINDOW: u64 = 1024;

/// A 32-byte per-direction secret.
pub type Secret = [u8; 32];

struct Len(usize);
impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(secret: &Secret, label: &[u8], out: &mut [u8]) {
    let prk = hkdf::Prk::new_less_safe(hkdf::HKDF_SHA256, secret);
    // Only fails for lengths > 255 * 32.
    prk.expand(&[label], Len(out.len()))
        .and_then(|okm| okm.fill(out))
        .expect("HKDF length");
}

fn next_secret(s: &Secret) -> Secret {
    let mut n = [0u8; 32];
    expand(s, b"glass media next", &mut n);
    n
}

struct EpochKey {
    key: LessSafeKey,
    iv: [u8; 12],
}

impl EpochKey {
    fn new(s: &Secret) -> Self {
        let mut k = [0u8; 16];
        let mut iv = [0u8; 12];
        expand(s, b"glass media key", &mut k);
        expand(s, b"glass media iv", &mut iv);
        let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &k).expect("AES-128 key"));
        Self { key, iv }
    }

    fn nonce(&self, counter: u64) -> Nonce {
        let mut n = self.iv;
        for (b, c) in n[4..].iter_mut().zip(counter.to_be_bytes()) {
            *b ^= c;
        }
        Nonce::assume_unique_for_key(n)
    }
}

fn aad(counter: u64, session: u32) -> [u8; 12] {
    let mut a = [0u8; 12];
    a[..8].copy_from_slice(&counter.to_be_bytes());
    a[8..].copy_from_slice(&session.to_be_bytes());
    a
}

/// Sending half of one direction.
pub struct Sealer {
    key: EpochKey,
    /// Secret of the next epoch.
    next: Secret,
    epoch: u64,
    counter: u64,
    session: u32,
    epoch_bits: u32,
}

impl Sealer {
    pub fn new(secret: Secret, session: u32) -> Self {
        Self::with_epoch_bits(secret, session, EPOCH_BITS)
    }

    fn with_epoch_bits(secret: Secret, session: u32, epoch_bits: u32) -> Self {
        Self {
            key: EpochKey::new(&secret),
            next: next_secret(&secret),
            epoch: 0,
            counter: 0,
            session,
            epoch_bits,
        }
    }

    /// Encrypts `plain` into `out` and returns the datagram length
    /// (`plain.len() + OVERHEAD`). Panics if `out` is too small.
    pub fn seal(&mut self, plain: &[u8], out: &mut [u8]) -> usize {
        self.counter += 1;
        let c = self.counter;
        while self.epoch < c >> self.epoch_bits {
            self.key = EpochKey::new(&self.next);
            self.next = next_secret(&self.next);
            self.epoch += 1;
        }
        let n = plain.len();
        out[..COUNTER].copy_from_slice(&c.to_be_bytes());
        let body = &mut out[COUNTER..COUNTER + n];
        body.copy_from_slice(plain);
        let tag = self
            .key
            .key
            .seal_in_place_separate_tag(self.key.nonce(c), Aad::from(aad(c, self.session)), body)
            .expect("AES-GCM seal");
        out[COUNTER + n..COUNTER + n + TAG].copy_from_slice(tag.as_ref());
        n + OVERHEAD
    }

    /// Counter of the last datagram sealed.
    pub fn counter(&self) -> u64 {
        self.counter
    }
}

/// Why `Opener::open` rejected a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// Too short, wrong key, altered, or from too far away an epoch.
    Bad,
    /// Already seen, or older than the replay window.
    Replayed,
}

/// Rejected-datagram counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct OpenStats {
    pub bad: u64,
    pub replayed: u64,
}

/// Receiving half of one direction.
pub struct Opener {
    key: EpochKey,
    /// Secret of epoch `epoch + 1`.
    next: Secret,
    epoch: u64,
    session: u32,
    epoch_bits: u32,
    /// Highest counter accepted (0 = none yet).
    max: u64,
    /// Bit `c % WINDOW` set = counter `c` seen (for `c` in `max-WINDOW+1 ..= max`).
    seen: [u64; (WINDOW / 64) as usize],
    pub stats: OpenStats,
}

impl Opener {
    pub fn new(secret: Secret, session: u32) -> Self {
        Self::with_epoch_bits(secret, session, EPOCH_BITS)
    }

    fn with_epoch_bits(secret: Secret, session: u32, epoch_bits: u32) -> Self {
        Self {
            key: EpochKey::new(&secret),
            next: next_secret(&secret),
            epoch: 0,
            session,
            epoch_bits,
            max: 0,
            seen: [0; (WINDOW / 64) as usize],
            stats: OpenStats::default(),
        }
    }

    fn bit(c: u64) -> (usize, u64) {
        let i = c % WINDOW;
        ((i / 64) as usize, 1 << (i % 64))
    }

    fn fresh(&self, c: u64) -> bool {
        if c > self.max {
            return true;
        }
        if self.max - c >= WINDOW {
            return false;
        }
        let (w, m) = Self::bit(c);
        self.seen[w] & m == 0
    }

    fn mark(&mut self, c: u64) {
        if c > self.max {
            if c - self.max >= WINDOW {
                self.seen = [0; (WINDOW / 64) as usize];
            } else {
                for k in self.max + 1..c {
                    let (w, m) = Self::bit(k);
                    self.seen[w] &= !m;
                }
            }
            self.max = c;
        }
        let (w, m) = Self::bit(c);
        self.seen[w] |= m;
    }

    /// Authenticates and decrypts `wire` in place; returns the plaintext
    /// (a sub-slice of `wire`).
    pub fn open<'a>(&mut self, wire: &'a mut [u8]) -> Result<&'a [u8], OpenError> {
        let r = self.open_inner(wire);
        match r {
            Err(OpenError::Bad) => self.stats.bad += 1,
            Err(OpenError::Replayed) => self.stats.replayed += 1,
            Ok(_) => {}
        }
        r
    }

    fn open_inner<'a>(&mut self, wire: &'a mut [u8]) -> Result<&'a [u8], OpenError> {
        if wire.len() < OVERHEAD {
            return Err(OpenError::Bad);
        }
        let c = u64::from_be_bytes(wire[..COUNTER].try_into().unwrap());
        if c == 0 {
            return Err(OpenError::Bad);
        }
        if !self.fresh(c) {
            return Err(OpenError::Replayed);
        }
        let e = c >> self.epoch_bits;
        // A later epoch: derive its key on trial, commit only if it opens.
        let later = if e == self.epoch {
            None
        } else if e > self.epoch && e - self.epoch <= MAX_EPOCH_JUMP {
            let mut s = self.next;
            for _ in self.epoch + 1..e {
                s = next_secret(&s);
            }
            Some((EpochKey::new(&s), s))
        } else {
            return Err(OpenError::Bad);
        };
        let key = later.as_ref().map_or(&self.key, |(k, _)| k);
        let len = wire.len() - OVERHEAD;
        key.key
            .open_in_place(
                key.nonce(c),
                Aad::from(aad(c, self.session)),
                &mut wire[COUNTER..],
            )
            .map_err(|_| OpenError::Bad)?;
        if let Some((k, s)) = later {
            self.key = k;
            self.next = next_secret(&s);
            self.epoch = e;
        }
        self.mark(c);
        Ok(&wire[COUNTER..COUNTER + len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(bits: u32) -> (Sealer, Opener) {
        let s = [7u8; 32];
        (
            Sealer::with_epoch_bits(s, 42, bits),
            Opener::with_epoch_bits(s, 42, bits),
        )
    }

    fn seal(s: &mut Sealer, msg: &[u8]) -> Vec<u8> {
        let mut out = [0u8; WIRE_MAX];
        let n = s.seal(msg, &mut out);
        out[..n].to_vec()
    }

    #[test]
    fn round_trip_and_overhead() {
        let (mut s, mut o) = pair(EPOCH_BITS);
        for len in [0usize, 1, 100, WIRE_MAX - OVERHEAD] {
            let msg: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let mut w = seal(&mut s, &msg);
            assert_eq!(w.len(), len + OVERHEAD);
            if len > 0 {
                assert_ne!(&w[COUNTER..COUNTER + len], &msg[..], "not encrypted");
            }
            assert_eq!(o.open(&mut w).unwrap(), &msg[..]);
        }
    }

    #[test]
    fn tampering_is_rejected() {
        let (mut s, mut o) = pair(EPOCH_BITS);
        let w = seal(&mut s, b"hello video");
        for i in 0..w.len() {
            let mut t = w.clone();
            t[i] ^= 0x01;
            assert!(o.open(&mut t).is_err(), "flipped byte {i} accepted");
        }
        // Truncated, and the untouched original still opens afterwards.
        assert_eq!(o.open(&mut w[..OVERHEAD - 1].to_vec()), Err(OpenError::Bad));
        assert!(o.open(&mut w.clone()).is_ok());
    }

    #[test]
    fn wrong_key_or_session_is_rejected() {
        let (mut s, _) = pair(EPOCH_BITS);
        let w = seal(&mut s, b"input");
        let mut other_key = Opener::new([8u8; 32], 42);
        let mut other_session = Opener::new([7u8; 32], 43);
        assert_eq!(other_key.open(&mut w.clone()), Err(OpenError::Bad));
        assert_eq!(other_session.open(&mut w.clone()), Err(OpenError::Bad));
    }

    #[test]
    fn replays_are_rejected_reordering_is_not() {
        let (mut s, mut o) = pair(EPOCH_BITS);
        let ws: Vec<Vec<u8>> = (0..2000).map(|i| seal(&mut s, &[i as u8; 10])).collect();
        // Out of order within the window: all accepted once.
        for i in [5usize, 3, 4, 0, 1, 2, 9, 8, 7, 6] {
            assert!(o.open(&mut ws[i].clone()).is_ok(), "packet {i}");
        }
        for w in ws.iter().take(10) {
            assert_eq!(o.open(&mut w.clone()), Err(OpenError::Replayed));
        }
        // Jump ahead; a packet more than WINDOW behind is refused even if new.
        assert!(o.open(&mut ws[1500].clone()).is_ok());
        assert_eq!(o.open(&mut ws[400].clone()), Err(OpenError::Replayed));
        assert!(o.open(&mut ws[1000].clone()).is_ok()); // within the window
        assert_eq!(o.open(&mut ws[1000].clone()), Err(OpenError::Replayed));
        assert_eq!(o.stats.replayed, 12);
    }

    #[test]
    fn counters_and_nonces_never_repeat() {
        let (mut s, _) = pair(4); // epochs of 16 packets
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let w = seal(&mut s, b"x");
            assert!(seen.insert(w[..COUNTER].to_vec()), "counter repeated");
        }
        assert_eq!(s.counter(), 1000);
    }

    #[test]
    fn keys_roll_over_and_receiver_follows() {
        let (mut s, mut o) = pair(4);
        let mut ws = Vec::new();
        for i in 0..200u32 {
            ws.push(seal(&mut s, &i.to_be_bytes()));
        }
        assert_eq!(s.epoch, 200 >> 4);
        // Every other packet, so epoch boundaries are crossed with gaps.
        for (i, w) in ws.iter().enumerate().step_by(2) {
            assert_eq!(o.open(&mut w.clone()).unwrap(), &(i as u32).to_be_bytes());
        }
        // Different epochs really use different keys.
        let mut t = ws[20].clone(); // counter 21, epoch 1
        t[..COUNTER].copy_from_slice(&5u64.to_be_bytes()); // relabelled as epoch 0
        let mut o2 = Opener::with_epoch_bits([7u8; 32], 42, 4);
        assert_eq!(o2.open(&mut t), Err(OpenError::Bad));
        // A packet from an epoch already left behind is refused.
        let (mut s3, mut o3) = pair(4);
        let w: Vec<Vec<u8>> = (0..40).map(|_| seal(&mut s3, b"y")).collect();
        assert!(o3.open(&mut w[39].clone()).is_ok()); // epoch 2
        assert!(o3.open(&mut w[10].clone()).is_err()); // epoch 0
    }

    #[test]
    fn receiver_catches_up_after_long_loss() {
        let (mut s, mut o) = pair(4);
        let mut last = Vec::new();
        for _ in 0..(16 * 10) {
            last = seal(&mut s, b"z");
        }
        assert!(o.open(&mut last).is_ok()); // 10 epochs ahead
        let mut way = Vec::new();
        for _ in 0..(16 * 40) {
            way = seal(&mut s, b"z");
        }
        assert_eq!(o.open(&mut way), Err(OpenError::Bad)); // beyond MAX_EPOCH_JUMP
    }

    /// Cost per 1400-byte datagram (run with `--ignored --nocapture`).
    #[test]
    #[ignore]
    fn bench_seal_open() {
        let (mut s, mut o) = pair(EPOCH_BITS);
        let msg = [0x5au8; WIRE_MAX - OVERHEAD];
        let mut out = [0u8; WIRE_MAX];
        let n = 200_000;
        let t = std::time::Instant::now();
        for _ in 0..n {
            let len = s.seal(&msg, &mut out);
            o.open(&mut out[..len]).unwrap();
        }
        let per = t.elapsed().as_nanos() as f64 / n as f64;
        println!(
            "seal+open {per:.0} ns per 1400-byte datagram ({:.0} MB/s)",
            1400.0 / per * 1e3
        );
    }
}
