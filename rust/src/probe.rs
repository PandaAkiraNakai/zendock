//! ¿Sirve una web este puerto? Un GET / que cuenta como web si responde HTML o
//! una redirección (una API que contesta JSON no cuenta). Prueba http y https;
//! el certificado no se valida: solo interesa saber si hay una página.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

const TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        use SignatureScheme::*;
        vec![
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ED25519,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
        ]
    }
}

fn tls_config() -> Option<Arc<ClientConfig>> {
    static CFG: OnceLock<Option<Arc<ClientConfig>>> = OnceLock::new();
    CFG.get_or_init(|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .ok()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        Some(Arc::new(cfg))
    })
    .clone()
}

enum Outcome {
    /// Rechazada, sin respuesta o colgó sin decir nada: puede estar arrancando
    NoAnswer,
    /// Contesta, pero en otro protocolo (p. ej. TLS en el http)
    OtherProtocol,
    Http {
        status: u16,
        ctype: String,
    },
}

fn read_head<R: Read>(r: &mut R) -> Outcome {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 32 * 1024 {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Outcome::OtherProtocol;
            }
            Err(_) => {
                if buf.is_empty() {
                    return Outcome::NoAnswer;
                }
                break;
            }
        }
    }
    if buf.is_empty() {
        return Outcome::NoAnswer;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let version = parts.next().unwrap_or("");
    let status = parts.next().and_then(|s| (s.len() == 3).then(|| s.parse::<u16>().ok()).flatten());
    let (true, Some(status)) = (version.starts_with("HTTP/"), status) else {
        return Outcome::OtherProtocol;
    };
    let ctype = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    Outcome::Http { status, ctype }
}

fn attempt(addr: SocketAddr, host_header: &str, tls: bool) -> Outcome {
    let Ok(tcp) = TcpStream::connect_timeout(&addr, TIMEOUT) else { return Outcome::NoAnswer };
    let _ = tcp.set_read_timeout(Some(TIMEOUT));
    let _ = tcp.set_write_timeout(Some(TIMEOUT));
    let req = format!(
        "GET / HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: ZenDock\r\nAccept: text/html\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    if !tls {
        let mut tcp = tcp;
        if tcp.write_all(req.as_bytes()).is_err() {
            return Outcome::NoAnswer;
        }
        return read_head(&mut tcp);
    }
    let Some(cfg) = tls_config() else { return Outcome::NoAnswer };
    let name = ServerName::from(addr.ip());
    let Ok(conn) = ClientConnection::new(cfg, name) else { return Outcome::NoAnswer };
    let mut s = StreamOwned::new(conn, tcp);
    while s.conn.is_handshaking() {
        if let Err(e) = s.conn.complete_io(&mut s.sock) {
            return match e.kind() {
                io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => Outcome::OtherProtocol,
                _ => Outcome::NoAnswer,
            };
        }
    }
    if s.write_all(req.as_bytes()).and_then(|_| s.flush()).is_err() {
        return Outcome::NoAnswer;
    }
    read_head(&mut s)
}

/// (url, definitivo). `definitivo` es false cuando nadie contestó: el servicio
/// puede estar arrancando (Apache tarda un segundo en abrir el puerto).
pub fn probe_web(host: &str, port: u16) -> (Option<String>, bool) {
    let url_host = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
    let Some(addr) = (host, port).to_socket_addrs().ok().and_then(|mut a| a.next()) else {
        return (None, false);
    };
    let header = format!("{url_host}:{port}");
    let schemes: [(&str, bool); 2] = if port.to_string().ends_with("443") {
        [("https", true), ("http", false)]
    } else {
        [("http", false), ("https", true)]
    };
    let mut answered = false;
    for (scheme, tls) in schemes {
        match attempt(addr, &header, tls) {
            Outcome::NoAnswer => continue,
            Outcome::OtherProtocol => answered = true,
            Outcome::Http { status, ctype } => {
                if (300..400).contains(&status) || (ctype.contains("html") && status < 500) {
                    return (Some(format!("{scheme}://{url_host}:{port}/")), true);
                }
                return (None, true); // habla HTTP pero no es una página: una API
            }
        }
    }
    (None, answered)
}
