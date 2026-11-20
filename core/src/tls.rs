//! TLS 1.3 for the control connection (rustls, ring provider).
//!
//! Both sides present their device certificate. The verifiers here check
//! only that the peer *holds the key* of the certificate it sent (the TLS 1.3
//! CertificateVerify signature); whether that certificate is one we've paired
//! with is decided right after the handshake against the pinned fingerprint
//! list (`identity.rs`), before any application message is acted on. There
//! is no CA and no name check: the fingerprint is the identity.
//!
//! The connection is shared between one reader thread and any number of
//! writers: [`TlsReader`] reads the socket without holding the lock and only
//! locks to feed bytes into rustls; [`TlsWriter`] locks to encrypt and send.

use crate::identity::{fingerprint, Fingerprint, Identity};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, ClientConnection, Connection, DigitallySignedStruct, DistinguishedName,
    ServerConfig, ServerConnection, SignatureScheme,
};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_err(e: rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Accepts any certificate whose key signed the handshake; pinning is checked
/// after the handshake.
#[derive(Debug)]
struct PinAfterHandshake(Arc<CryptoProvider>);

impl PinAfterHandshake {
    fn tls13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn no_tls12() -> rustls::Error {
    rustls::Error::General("TLS 1.2 is not used".into())
}

impl ServerCertVerifier for PinAfterHandshake {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(no_tls12())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for PinAfterHandshake {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(no_tls12())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

fn cert_and_key(id: &Identity) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    (
        vec![CertificateDer::from(id.cert.clone())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key_der().to_vec())),
    )
}

pub(crate) fn server_config(id: &Identity) -> io::Result<Arc<ServerConfig>> {
    let p = provider();
    let (certs, key) = cert_and_key(id);
    let mut c = ServerConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_err)?
        .with_client_cert_verifier(Arc::new(PinAfterHandshake(p)))
        .with_single_cert(certs, key)
        .map_err(tls_err)?;
    // Every connection is a full handshake with both certificates.
    c.send_tls13_tickets = 0;
    c.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    Ok(Arc::new(c))
}

pub(crate) fn client_config(id: &Identity) -> io::Result<Arc<ClientConfig>> {
    let p = provider();
    let (certs, key) = cert_and_key(id);
    let mut c = ClientConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_err)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinAfterHandshake(p)))
        .with_client_auth_cert(certs, key)
        .map_err(tls_err)?;
    c.resumption = rustls::client::Resumption::disabled();
    Ok(Arc::new(c))
}

pub(crate) fn server_conn(cfg: &Arc<ServerConfig>) -> io::Result<Connection> {
    Ok(ServerConnection::new(cfg.clone()).map_err(tls_err)?.into())
}

pub(crate) fn client_conn(cfg: &Arc<ClientConfig>) -> io::Result<Connection> {
    // The name is never checked (see the module docs).
    let name = ServerName::try_from("glass.invalid").expect("valid name");
    Ok(ClientConnection::new(cfg.clone(), name)
        .map_err(tls_err)?
        .into())
}

struct Shared {
    conn: Connection,
    out: TcpStream,
}

impl Shared {
    fn flush(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.out)?;
        }
        Ok(())
    }
}

fn lock(m: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Encrypting writer; clone freely across threads.
#[derive(Clone)]
pub(crate) struct TlsWriter(Arc<Mutex<Shared>>);

impl TlsWriter {
    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut s = lock(&self.0);
        s.conn.writer().write_all(bytes)?;
        s.flush()
    }

    /// RFC 5705/8446 exporter: `len` bytes bound to this TLS session.
    pub fn export(&self, label: &[u8], context: &[u8], out: &mut [u8]) -> io::Result<()> {
        lock(&self.0)
            .conn
            .export_keying_material(out, label, Some(context))
            .map(|_| ())
            .map_err(tls_err)
    }
}

/// Decrypting reader for one thread. Honours the socket's read timeout
/// (returns `WouldBlock`/`TimedOut` errors like a `TcpStream`).
pub(crate) struct TlsReader {
    sh: Arc<Mutex<Shared>>,
    rd: TcpStream,
    raw: Box<[u8]>,
    pos: usize,
    len: usize,
}

impl TlsReader {
    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.rd.set_read_timeout(d)
    }
}

impl Read for TlsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            {
                let mut s = lock(&self.sh);
                match s.conn.reader().read(buf) {
                    Ok(n) => return Ok(n), // 0 = peer closed cleanly
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
                if self.pos < self.len {
                    let mut src = &self.raw[self.pos..self.len];
                    let used = s.conn.read_tls(&mut src)?;
                    self.pos += used;
                    s.conn.process_new_packets().map_err(|e| {
                        let _ = s.flush(); // send the alert
                        tls_err(e)
                    })?;
                    s.flush()?; // key updates etc.
                    continue;
                }
            }
            // Nothing buffered: wait for the socket without holding the lock.
            let n = self.rd.read(&mut self.raw)?;
            if n == 0 {
                return Ok(0);
            }
            self.pos = 0;
            self.len = n;
        }
    }
}

/// One TLS connection after the handshake.
pub(crate) struct Channel {
    pub reader: TlsReader,
    pub writer: TlsWriter,
    /// Fingerprint of the certificate the peer proved it holds.
    pub peer: Fingerprint,
}

/// Runs the handshake on `tcp` (using its current read timeout) and splits
/// the connection into a reader and a writer.
pub(crate) fn handshake(mut tcp: TcpStream, mut conn: Connection) -> io::Result<Channel> {
    while conn.is_handshaking() {
        let (rd, wr) = conn.complete_io(&mut tcp)?;
        if rd == 0 && wr == 0 && conn.is_handshaking() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    let peer = conn
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| fingerprint(c.as_ref()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "peer sent no certificate"))?;
    let rd = tcp.try_clone()?;
    let sh = Arc::new(Mutex::new(Shared { conn, out: tcp }));
    Ok(Channel {
        reader: TlsReader {
            sh: sh.clone(),
            rd,
            raw: vec![0u8; 32 * 1024].into_boxed_slice(),
            pos: 0,
            len: 0,
        },
        writer: TlsWriter(sh),
        peer,
    })
}
