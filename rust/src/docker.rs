//! Cliente mínimo de la API del Engine de Docker sobre su socket unix: HTTP/1.1
//! a mano, con cuerpos `chunked` (los usan /events y a veces /stats).
//!
//! Ninguna llamada abre el socket si `Gate` dice que dockerd no está en marcha:
//! con activación por socket, conectarse bastaría para encenderlo.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::gate::Gate;

pub const APAGADO: &str = "Docker está apagado";

pub struct Docker {
    pub path: PathBuf,
    gate: Arc<Gate>,
}

/// Lee un cuerpo con `Transfer-Encoding: chunked` como un flujo normal.
pub struct Chunked<R: BufRead> {
    inner: R,
    left: usize,
    done: bool,
}

impl<R: BufRead> Chunked<R> {
    fn new(inner: R) -> Self {
        Self { inner, left: 0, done: false }
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            if self.inner.read_line(&mut line)? == 0 {
                self.done = true;
                return Ok(0);
            }
            let hex = line.trim().split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(hex, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunk inválido"))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.left = size;
        }
        let want = buf.len().min(self.left);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            self.done = true;
            return Ok(0);
        }
        self.left -= n;
        if self.left == 0 {
            let mut crlf = String::new();
            self.inner.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

pub enum Body {
    Chunked(Chunked<BufReader<UnixStream>>),
    Sized(io::Take<BufReader<UnixStream>>),
    Rest(BufReader<UnixStream>),
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Body::Chunked(r) => r.read(buf),
            Body::Sized(r) => r.read(buf),
            Body::Rest(r) => r.read(buf),
        }
    }
}

fn socket_error(e: &io::Error, path: &std::path::Path) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => {
            format!("sin permiso para {} (¿tu usuario está en el grupo docker?)", path.display())
        }
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => "Docker no está en marcha".into(),
        _ => format!("sin conexión con Docker ({e})"),
    }
}

impl Docker {
    pub fn new(path: PathBuf, gate: Arc<Gate>) -> Self {
        Self { path, gate }
    }

    pub fn socket_from_env() -> PathBuf {
        match std::env::var("DOCKER_HOST") {
            Ok(h) if h.starts_with("unix://") => PathBuf::from(&h["unix://".len()..]),
            _ => PathBuf::from("/var/run/docker.sock"),
        }
    }

    /// Abre la conexión, manda la petición y deja el cuerpo listo para leer.
    fn open(
        &self,
        method: &str,
        url: &str,
        timeout: Option<Duration>,
        gated: bool,
    ) -> Result<(u16, Body), String> {
        if gated && !self.gate.is_on() {
            return Err(APAGADO.into());
        }
        let stream = UnixStream::connect(&self.path).map_err(|e| socket_error(&e, &self.path))?;
        stream.set_read_timeout(timeout).ok();
        stream.set_write_timeout(timeout.or(Some(Duration::from_secs(10)))).ok();
        let extra = if method == "POST" { "Content-Length: 0\r\n" } else { "" };
        let req = format!(
            "{method} {url} HTTP/1.1\r\nHost: docker\r\nUser-Agent: ZenDock\r\nConnection: close\r\n{extra}\r\n"
        );
        (&stream).write_all(req.as_bytes()).map_err(|e| socket_error(&e, &self.path))?;

        let mut reader = BufReader::with_capacity(16 * 1024, stream);
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| format!("sin conexión con Docker ({e})"))?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| "respuesta de Docker ilegible".to_string())?;
        let mut chunked = false;
        let mut length: Option<u64> = None;
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(|e| format!("sin conexión con Docker ({e})"))?;
            let l = line.trim_end();
            if n == 0 || l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                let v = v.trim();
                if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
                    chunked = true;
                } else if k.eq_ignore_ascii_case("content-length") {
                    length = v.parse().ok();
                }
            }
        }
        let body = if chunked {
            Body::Chunked(Chunked::new(reader))
        } else if let Some(n) = length {
            Body::Sized(reader.take(n))
        } else {
            Body::Rest(reader)
        };
        Ok((status, body))
    }

    fn finish(&self, status: u16, mut body: Body) -> Result<Option<Value>, String> {
        let mut data = Vec::new();
        body.read_to_end(&mut data).map_err(|e| format!("sin conexión con Docker ({e})"))?;
        if status >= 400 {
            let msg = serde_json::from_slice::<Value>(&data)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&data).trim().to_owned());
            return Err(if msg.is_empty() { format!("HTTP {status}") } else { msg });
        }
        if data.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&data).map(Some).map_err(|e| format!("JSON de Docker ilegible ({e})"))
    }

    pub fn call(&self, method: &str, url: &str, timeout: Duration) -> Result<Option<Value>, String> {
        let (status, body) = self.open(method, url, Some(timeout), true)?;
        self.finish(status, body)
    }

    pub fn get(&self, url: &str) -> Result<Value, String> {
        self.call("GET", url, Duration::from_secs(10))?.ok_or_else(|| "respuesta vacía de Docker".into())
    }

    /// Flujo largo (/events): cada línea es un objeto JSON.
    pub fn stream(&self, url: &str) -> Result<BufReader<Body>, String> {
        let (status, body) = self.open("GET", url, None, true)?;
        if status >= 400 {
            return Err(self.finish(status, body).err().unwrap_or(format!("HTTP {status}")));
        }
        Ok(BufReader::with_capacity(8 * 1024, body))
    }

    /// GET /_ping sin mirar `Gate`: con activación por socket es lo que enciende Docker.
    pub fn ping(&self) -> Result<(), String> {
        let (status, body) = self.open("GET", "/_ping", Some(Duration::from_secs(120)), false)?;
        let mut data = Vec::new();
        let _ = Body::read_to_end(&mut { body }, &mut data);
        if status >= 400 {
            return Err(format!("Docker contestó HTTP {status}"));
        }
        Ok(())
    }
}
