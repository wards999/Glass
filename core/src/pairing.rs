//! PIN pairing: SPAKE2 (Ed25519 group) inside a TLS 1.3 connection whose
//! certificates haven't been pinned yet.
//!
//! - The host shows a 6-digit PIN; the viewer's user types it.
//! - SPAKE2 identities are both certificate fingerprints, so a
//!   man-in-the-middle (who must present its own certificate on each side)
//!   ends up with different keys on each side unless it guesses the PIN, and
//!   it gets one guess per attempt.
//! - Key confirmation MACs also cover a TLS exporter value, binding the
//!   result to this exact TLS session.
//! - The PIN never crosses the network, and a passive listener learns
//!   nothing it can test offline.
//! - The host counts an attempt *before* the exchange (an abandoned attempt
//!   still counts): 3 wrong per PIN, then a new PIN; 10 wrong in total and
//!   pairing is off until the host restarts. An online attacker gets at most
//!   10 guesses in 10^6 per host run.

use crate::identity::{random_pin, Fingerprint};
use ring::hmac;
use spake2::{Ed25519Group, Identity, Password, Spake2};

/// TLS exporter label for the confirmation MACs.
pub(crate) const EXPORT_LABEL: &[u8] = b"EXPORTER-glass-pairing";
pub const PIN_LIFETIME_US: u64 = 120_000_000;
pub const WRONG_PER_PIN: u32 = 3;
pub const WRONG_TOTAL: u32 = 10;

fn ids(viewer: &Fingerprint, host: &Fingerprint) -> (Identity, Identity) {
    let mut a = b"glass-viewer:".to_vec();
    a.extend_from_slice(viewer);
    let mut b = b"glass-host:".to_vec();
    b.extend_from_slice(host);
    (Identity::new(&a), Identity::new(&b))
}

/// SPAKE2 state for one side.
pub(crate) struct Pake(Spake2<Ed25519Group>);

/// Starts the exchange; returns our state and the message to send.
pub(crate) fn start(
    is_host: bool,
    pin: &str,
    viewer: &Fingerprint,
    host: &Fingerprint,
) -> (Pake, Vec<u8>) {
    let (a, b) = ids(viewer, host);
    let pw = Password::new(pin.trim().as_bytes());
    let (s, msg) = if is_host {
        Spake2::<Ed25519Group>::start_b(&pw, &a, &b)
    } else {
        Spake2::<Ed25519Group>::start_a(&pw, &a, &b)
    };
    (Pake(s), msg)
}

/// Both confirmation MACs: (viewer's, host's).
pub(crate) struct Confirm {
    key: hmac::Key,
    binding: Vec<u8>,
}

impl Pake {
    /// Finishes with the peer's message. `binding` = TLS exporter output.
    /// `None` if the message is malformed (counts as a wrong PIN).
    pub fn finish(self, peer_msg: &[u8], binding: &[u8]) -> Option<Confirm> {
        let k = self.0.finish(peer_msg).ok()?;
        Some(Confirm {
            key: hmac::Key::new(hmac::HMAC_SHA256, &k),
            binding: binding.to_vec(),
        })
    }
}

impl Confirm {
    fn data(&self, who: &[u8]) -> Vec<u8> {
        let mut d = who.to_vec();
        d.extend_from_slice(&self.binding);
        d
    }

    pub fn mac(&self, from_host: bool) -> Vec<u8> {
        let who: &[u8] = if from_host { b"host" } else { b"viewer" };
        hmac::sign(&self.key, &self.data(who)).as_ref().to_vec()
    }

    /// Constant-time check of the peer's MAC.
    pub fn verify(&self, from_host: bool, mac: &[u8]) -> bool {
        let who: &[u8] = if from_host { b"host" } else { b"viewer" };
        hmac::verify(&self.key, &self.data(who), mac).is_ok()
    }
}

struct ActivePin {
    pin: String,
    created_us: u64,
    wrong: u32,
}

/// Host-side PIN and attempt bookkeeping (lives as long as the listener).
pub(crate) struct PinState {
    fixed: Option<String>,
    current: Option<ActivePin>,
    pub wrong_total: u32,
}

impl PinState {
    pub fn new(fixed: Option<String>) -> Self {
        Self {
            fixed,
            current: None,
            wrong_total: 0,
        }
    }

    pub fn locked(&self) -> bool {
        self.wrong_total >= WRONG_TOTAL
    }

    fn expire(&mut self, now: u64) {
        if let Some(p) = &self.current {
            let old = self.fixed.is_none() && now.saturating_sub(p.created_us) > PIN_LIFETIME_US;
            if old || p.wrong >= WRONG_PER_PIN {
                self.current = None;
            }
        }
    }

    /// The PIN to show for a new request; `true` if it's a new one.
    pub fn for_request(&mut self, now: u64) -> (String, bool) {
        self.expire(now);
        if let Some(p) = &self.current {
            return (p.pin.clone(), false);
        }
        let pin = self.fixed.clone().unwrap_or_else(random_pin);
        self.current = Some(ActivePin {
            pin: pin.clone(),
            created_us: now,
            wrong: 0,
        });
        (pin, true)
    }

    /// Microseconds the current PIN stays valid.
    pub fn remaining_us(&self, now: u64) -> u64 {
        match (&self.current, &self.fixed) {
            (Some(_), Some(_)) => PIN_LIFETIME_US,
            (Some(p), None) => (p.created_us + PIN_LIFETIME_US).saturating_sub(now),
            (None, _) => 0,
        }
    }

    /// Counts an attempt up front and returns the PIN to check against, or
    /// `None` if there's no valid PIN (expired, used up, locked).
    pub fn begin_attempt(&mut self, now: u64) -> Option<String> {
        self.expire(now);
        if self.locked() {
            return None;
        }
        let p = self.current.as_mut()?;
        p.wrong += 1;
        self.wrong_total += 1;
        Some(p.pin.clone())
    }

    /// The attempt succeeded: refund it and retire the PIN.
    pub fn succeeded(&mut self) {
        self.wrong_total = self.wrong_total.saturating_sub(1);
        self.current = None;
    }

    /// Wrong guesses left for the current PIN.
    pub fn left_for_pin(&self) -> u32 {
        self.current
            .as_ref()
            .map_or(0, |p| WRONG_PER_PIN.saturating_sub(p.wrong))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(host_pin: &str, viewer_pin: &str, fps: [[u8; 32]; 4], bind: [&[u8]; 2]) -> bool {
        // fps: viewer's view (viewer, host), host's view (viewer, host).
        let (h, hm) = start(true, host_pin, &fps[2], &fps[3]);
        let (v, vm) = start(false, viewer_pin, &fps[0], &fps[1]);
        let (Some(hc), Some(vc)) = (h.finish(&vm, bind[1]), v.finish(&hm, bind[0])) else {
            return false;
        };
        let ok_host = hc.verify(false, &vc.mac(false));
        let ok_viewer = vc.verify(true, &hc.mac(true));
        assert_eq!(ok_host, ok_viewer);
        ok_host
    }

    const V: [u8; 32] = [1; 32];
    const H: [u8; 32] = [2; 32];

    #[test]
    fn right_pin_pairs_wrong_pin_fails() {
        assert!(run("123456", "123456", [V, H, V, H], [b"tls", b"tls"]));
        assert!(run("123456", " 123456 ", [V, H, V, H], [b"tls", b"tls"]));
        assert!(!run("123456", "123457", [V, H, V, H], [b"tls", b"tls"]));
    }

    #[test]
    fn man_in_the_middle_fails_even_knowing_the_pin_flow() {
        // The attacker relays with its own certificate M on each side: the
        // viewer sees host M, the host sees viewer M.
        let m = [3u8; 32];
        assert!(!run("123456", "123456", [V, m, m, H], [b"tls", b"tls"]));
        // Same fingerprints but two different TLS sessions (exporters differ).
        assert!(!run("123456", "123456", [V, H, V, H], [b"tls-1", b"tls-2"]));
    }

    #[test]
    fn garbage_message_fails() {
        let (h, _) = start(true, "1", &V, &H);
        assert!(h.finish(&[0u8; 5], b"x").is_none());
    }

    #[test]
    fn attempt_limits() {
        let mut s = PinState::new(None);
        assert!(s.begin_attempt(0).is_none(), "no PIN shown yet");
        let (pin, new) = s.for_request(0);
        assert!(new);
        assert_eq!(s.for_request(1).0, pin, "same PIN while valid");
        for _ in 0..WRONG_PER_PIN {
            assert_eq!(s.begin_attempt(2).as_deref(), Some(pin.as_str()));
        }
        assert!(s.begin_attempt(3).is_none(), "PIN used up");
        let (pin2, new2) = s.for_request(4);
        assert!(new2);
        // Expiry.
        assert!(s.begin_attempt(5 + PIN_LIFETIME_US).is_none());
        // Success refunds and retires the PIN.
        let (p3, _) = s.for_request(10 * PIN_LIFETIME_US);
        let before = s.wrong_total;
        assert!(s.begin_attempt(10 * PIN_LIFETIME_US).is_some());
        s.succeeded();
        assert_eq!(s.wrong_total, before);
        assert_ne!(s.for_request(10 * PIN_LIFETIME_US).0, "", "{p3} {pin2}");
        // Lock after WRONG_TOTAL.
        let mut s = PinState::new(Some("111111".into()));
        let mut n = 0;
        loop {
            s.for_request(0);
            if s.begin_attempt(0).is_none() {
                break;
            }
            n += 1;
        }
        assert_eq!(n, WRONG_TOTAL);
        assert!(s.locked());
    }
}
