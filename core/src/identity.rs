//! Device identity and the list of paired devices.
//!
//! Each machine has one long-lived identity: an ECDSA P-256 key and a
//! self-signed certificate, made on first run. A device is known by the
//! SHA-256 fingerprint of its certificate. Pairing (`pairing.rs`) swaps
//! certificates under a PIN; afterwards every connection is mutual TLS and
//! each side checks the other's fingerprint against this list.
//!
//! Storage (one folder, [`default_dir`]):
//! - `identity.key`: certificate + private key, passed through the app's
//!   [`KeyProtector`] (DPAPI on Windows); on Unix the file is mode 0600 in a
//!   0700 folder.
//! - `peers.txt`: one line per paired device, `fingerprint roles name`.
//!   Re-read on every lookup, so `glass forget` takes effect on a running host.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub type Fingerprint = [u8; 32];

/// We paired with it as a viewer: it's a host we trust.
pub const ROLE_HOST: u8 = 1 << 0;
/// It paired with us while we were hosting: it may view and control this machine.
pub const ROLE_VIEWER: u8 = 1 << 1;

/// Longest device name, bytes.
pub const NAME_MAX: usize = 63;

pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    let d = ring::digest::digest(&ring::digest::SHA256, cert_der);
    d.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

pub fn fingerprint_hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Short form for people: first 8 bytes, "3f2a 91c0 77de 10ab".
pub fn fingerprint_short(fp: &Fingerprint) -> String {
    fp[..8]
        .chunks(2)
        .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_hex(s: &str) -> Option<Fingerprint> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut fp = [0u8; 32];
    for (i, b) in fp.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(fp)
}

/// Printable, single line, at most [`NAME_MAX`] bytes.
pub fn clean_name(s: &str) -> String {
    let mut out = String::new();
    for ch in s.trim().chars().filter(|c| !c.is_control()) {
        if out.len() + ch.len_utf8() > NAME_MAX {
            break;
        }
        out.push(ch);
    }
    if out.is_empty() {
        "Glass device".into()
    } else {
        out
    }
}

/// This machine's certificate and private key.
pub struct Identity {
    pub cert: Vec<u8>,
    key: Vec<u8>,
    pub fingerprint: Fingerprint,
}

const ID_MAGIC: &[u8; 4] = b"GLID";

impl Identity {
    pub fn generate() -> io::Result<Self> {
        let ck = rcgen::generate_simple_self_signed(vec!["glass.invalid".to_string()])
            .map_err(io::Error::other)?;
        let cert = ck.cert.der().to_vec();
        Ok(Self {
            fingerprint: fingerprint(&cert),
            key: ck.signing_key.serialize_der(),
            cert,
        })
    }

    /// PKCS#8 DER private key.
    pub(crate) fn key_der(&self) -> &[u8] {
        &self.key
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = ID_MAGIC.to_vec();
        for part in [&self.cert, &self.key] {
            b.extend_from_slice(&(part.len() as u32).to_be_bytes());
            b.extend_from_slice(part);
        }
        b
    }

    fn decode(b: &[u8]) -> Option<Self> {
        let mut rest = b.strip_prefix(ID_MAGIC)?;
        let mut part = || -> Option<Vec<u8>> {
            let n = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
            let p = rest.get(4..4 + n)?.to_vec();
            rest = &rest[4 + n..];
            Some(p)
        };
        let cert = part()?;
        let key = part()?;
        Some(Self {
            fingerprint: fingerprint(&cert),
            cert,
            key,
        })
    }
}

/// A paired device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub fingerprint: Fingerprint,
    pub name: String,
    /// `ROLE_*` bits.
    pub roles: u8,
}

/// Encrypts the private key at rest (e.g. Windows DPAPI). Both must round-trip.
pub struct KeyProtector {
    pub protect: fn(&[u8]) -> io::Result<Vec<u8>>,
    pub unprotect: fn(&[u8]) -> io::Result<Vec<u8>>,
}

/// This machine: its name, identity and paired devices.
pub struct Device {
    pub name: String,
    pub identity: Identity,
    dir: Option<PathBuf>,
    /// The list itself when in memory; a cache of `peers.txt` otherwise.
    peers: Mutex<Vec<Peer>>,
    /// "Seen by" (in memory; `seen-by.txt` shares it between processes).
    seen: Mutex<(u64, Vec<SeenId>)>,
}

/// A device in a "seen by" list: the first 8 bytes of its fingerprint.
pub type SeenId = [u8; 8];

/// The `SeenId` of a fingerprint.
pub fn seen_id(fp: &Fingerprint) -> SeenId {
    let mut id = [0u8; 8];
    id.copy_from_slice(&fp[..8]);
    id
}

fn seen_from_hex(s: &str) -> Option<SeenId> {
    if s.len() != 16 {
        return None;
    }
    let mut id = [0u8; 8];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(id)
}

/// A "seen by" record older than this is stale (its host process died).
const SEEN_FRESH_SECS: u64 = 10;

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `GLASS_HOME` if set, else the per-user app-data folder:
/// `%APPDATA%\Glass`, `~/Library/Application Support/Glass`, `~/.config/glass`.
pub fn default_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("GLASS_HOME") {
        return Some(PathBuf::from(h));
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Glass"));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Application Support/Glass"))
    } else {
        Some(home.join(".config/glass"))
    }
}

/// Writes `data` to `path` via a temporary file and rename, private on Unix.
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

fn parse_peers(text: &str) -> Vec<Peer> {
    let mut v = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(3, ' ');
        let (Some(fp), Some(roles), name) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let Some(fingerprint) = parse_hex(fp) else {
            continue;
        };
        let mut r = 0;
        for role in roles.split(',') {
            match role {
                "host" => r |= ROLE_HOST,
                "viewer" => r |= ROLE_VIEWER,
                _ => {}
            }
        }
        if r != 0 {
            v.push(Peer {
                fingerprint,
                name: clean_name(name.unwrap_or("")),
                roles: r,
            });
        }
    }
    v
}

fn format_peers(peers: &[Peer]) -> String {
    let mut s = String::from(
        "# Glass paired devices: fingerprint, roles, name.\n\
         # host = this machine views it; viewer = it may view and control this machine.\n\
         # Delete a line (or run `glass forget NAME`) to unpair.\n",
    );
    for p in peers {
        let mut roles = Vec::new();
        if p.roles & ROLE_HOST != 0 {
            roles.push("host");
        }
        if p.roles & ROLE_VIEWER != 0 {
            roles.push("viewer");
        }
        s += &format!(
            "{} {} {}\n",
            fingerprint_hex(&p.fingerprint),
            roles.join(","),
            p.name
        );
    }
    s
}

impl Device {
    /// Loads this machine's identity from `dir`, creating it (and the folder)
    /// on first run.
    pub fn open(dir: &Path, name: &str, protector: Option<&KeyProtector>) -> io::Result<Arc<Self>> {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir)?;
        let key_path = dir.join("identity.key");
        let identity = match std::fs::read(&key_path) {
            Ok(stored) => {
                let plain = match protector {
                    Some(p) => (p.unprotect)(&stored)?,
                    None => stored,
                };
                Identity::decode(&plain).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} is damaged", key_path.display()),
                    )
                })?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = Identity::generate()?;
                let plain = id.encode();
                let stored = match protector {
                    Some(p) => (p.protect)(&plain)?,
                    None => plain,
                };
                write_private(&key_path, &stored)?;
                id
            }
            Err(e) => return Err(e),
        };
        let d = Arc::new(Self {
            name: clean_name(name),
            identity,
            dir: Some(dir.to_path_buf()),
            peers: Mutex::new(Vec::new()),
            seen: Mutex::new((0, Vec::new())),
        });
        d.peers();
        Ok(d)
    }

    /// An identity and peer list that live only in memory (tests, replay).
    pub fn ephemeral(name: &str) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            name: clean_name(name),
            identity: Identity::generate()?,
            dir: None,
            peers: Mutex::new(Vec::new()),
            seen: Mutex::new((0, Vec::new())),
        }))
    }

    /// The machines that currently see this one: its viewers and, through
    /// them, theirs (no-loop rule, M3d). Shared by this machine's Glass
    /// processes: a host process records it while a session runs and
    /// refreshes it every few seconds; stale records are ignored.
    pub fn seen_by(&self) -> Vec<SeenId> {
        let (at, ids) = match &self.dir {
            Some(dir) => {
                let text = std::fs::read_to_string(dir.join("seen-by.txt")).unwrap_or_default();
                let mut lines = text.lines();
                let at = lines
                    .next()
                    .and_then(|l| l.trim().parse().ok())
                    .unwrap_or(0);
                let ids = lines.filter_map(|l| seen_from_hex(l.trim())).collect();
                (at, ids)
            }
            None => lock(&self.seen).clone(),
        };
        if unix_secs().saturating_sub(at) > SEEN_FRESH_SECS {
            Vec::new()
        } else {
            ids
        }
    }

    /// Records (or, with an empty list, clears) who sees this machine.
    /// Call again every few seconds while it holds.
    pub fn set_seen_by(&self, ids: &[SeenId]) {
        let at = if ids.is_empty() { 0 } else { unix_secs() };
        *lock(&self.seen) = (at, ids.to_vec());
        if let Some(dir) = &self.dir {
            let mut text = format!("{at}\n");
            for id in ids {
                text.extend(id.iter().map(|b| format!("{b:02x}")));
                text.push('\n');
            }
            let _ = write_private(&dir.join("seen-by.txt"), text.as_bytes());
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.identity.fingerprint
    }

    fn peers_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join("peers.txt"))
    }

    /// All paired devices (re-read from disk when file-backed).
    pub fn peers(&self) -> Vec<Peer> {
        let mut cache = lock(&self.peers);
        if let Some(p) = self.peers_path() {
            match std::fs::read_to_string(&p) {
                Ok(t) => *cache = parse_peers(&t),
                Err(e) if e.kind() == io::ErrorKind::NotFound => cache.clear(),
                Err(_) => {} // keep the last good copy
            }
        }
        cache.clone()
    }

    /// Paired devices to show the user: all but this device itself. (A
    /// device can pair with itself, connecting to its own address while
    /// testing: QUIETPC22 listed "QUIETPC22" among its paired devices.)
    pub fn listed_peers(&self) -> Vec<Peer> {
        let me = self.fingerprint();
        self.peers()
            .into_iter()
            .filter(|p| p.fingerprint != me)
            .collect()
    }

    /// The paired device with this fingerprint, if it has `role`.
    pub fn peer_with(&self, fp: &Fingerprint, role: u8) -> Option<Peer> {
        self.peers()
            .into_iter()
            .find(|p| &p.fingerprint == fp && p.roles & role != 0)
    }

    fn update(&self, f: impl FnOnce(&mut Vec<Peer>)) -> io::Result<()> {
        let mut peers = self.peers();
        f(&mut peers);
        if let Some(p) = self.peers_path() {
            write_private(&p, format_peers(&peers).as_bytes())?;
        }
        *lock(&self.peers) = peers;
        Ok(())
    }

    /// Records a pairing: adds `role` to the device (adding it if new) and
    /// updates its name.
    pub fn add_peer(&self, fp: Fingerprint, name: &str, role: u8) -> io::Result<()> {
        let name = clean_name(name);
        self.update(
            |peers| match peers.iter_mut().find(|p| p.fingerprint == fp) {
                Some(p) => {
                    p.roles |= role;
                    p.name = name;
                }
                None => peers.push(Peer {
                    fingerprint: fp,
                    name,
                    roles: role,
                }),
            },
        )
    }

    /// Unpairs a device (all roles). False if it wasn't paired.
    pub fn forget(&self, fp: &Fingerprint) -> io::Result<bool> {
        let mut found = false;
        self.update(|peers| {
            let before = peers.len();
            peers.retain(|p| &p.fingerprint != fp);
            found = peers.len() != before;
        })?;
        Ok(found)
    }

    /// Devices whose name (case-insensitive) or fingerprint prefix matches `what`.
    pub fn find(&self, what: &str) -> Vec<Peer> {
        let w = what.trim().to_lowercase().replace(' ', "");
        self.peers()
            .into_iter()
            .filter(|p| {
                p.name.to_lowercase() == what.trim().to_lowercase()
                    || (w.len() >= 4 && fingerprint_hex(&p.fingerprint).starts_with(&w))
            })
            .collect()
    }
}

/// A uniformly random 6-digit PIN from the OS RNG.
pub fn random_pin() -> String {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    loop {
        let mut b = [0u8; 4];
        rng.fill(&mut b).expect("OS random");
        let v = u32::from_be_bytes(b);
        // Reject the top sliver so every PIN is equally likely.
        if v < 4_294_000_000 {
            return format!("{:06}", v % 1_000_000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "glass-test-{tag}-{}-{}",
            std::process::id(),
            crate::clock::now_us()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn identity_persists_and_peers_round_trip() {
        let dir = temp_dir("id");
        let d = Device::open(&dir, "Test PC", None).unwrap();
        let fp = d.fingerprint();
        assert_eq!(fingerprint(&d.identity.cert), fp);
        let other = [9u8; 32];
        d.add_peer(other, "Simon's Mac\nevil", ROLE_VIEWER).unwrap();
        d.add_peer(other, "Simon's Mac", ROLE_HOST).unwrap();
        // Reopen: same identity, same peer list.
        let d2 = Device::open(&dir, "Test PC", None).unwrap();
        assert_eq!(d2.fingerprint(), fp);
        let peers = d2.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "Simon's Mac");
        assert_eq!(peers[0].roles, ROLE_HOST | ROLE_VIEWER);
        assert!(d2.peer_with(&other, ROLE_VIEWER).is_some());
        assert_eq!(d2.find("simon's mac").len(), 1);
        assert_eq!(d2.find("0909").len(), 1);
        // Forgetting in one handle is seen by the other (file re-read).
        assert!(d2.forget(&other).unwrap());
        assert!(d.peer_with(&other, ROLE_VIEWER).is_none());
        assert!(!d.forget(&other).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn protector_is_used_and_damage_detected() {
        fn flip(b: &[u8]) -> io::Result<Vec<u8>> {
            Ok(b.iter().map(|x| x ^ 0xa5).collect())
        }
        let p = KeyProtector {
            protect: flip,
            unprotect: flip,
        };
        let dir = temp_dir("prot");
        let fp = Device::open(&dir, "x", Some(&p)).unwrap().fingerprint();
        let raw = std::fs::read(dir.join("identity.key")).unwrap();
        assert!(!raw.starts_with(ID_MAGIC), "stored unprotected");
        assert_eq!(Device::open(&dir, "x", Some(&p)).unwrap().fingerprint(), fp);
        assert!(Device::open(&dir, "x", None).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("identity.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "key readable by others");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_and_pins() {
        assert_eq!(clean_name("  a\tb\n "), "ab");
        assert_eq!(clean_name(""), "Glass device");
        assert!(clean_name(&"é".repeat(100)).len() <= NAME_MAX);
        for _ in 0..100 {
            let p = random_pin();
            assert_eq!(p.len(), 6);
            assert!(p.bytes().all(|b| b.is_ascii_digit()));
        }
        assert_eq!(parse_peers("zz host x\n# c\n\n"), vec![]);
    }
}
