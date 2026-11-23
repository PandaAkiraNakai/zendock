//! Modelo: un contenedor tal como lo pinta ZenDock, su estado en castellano y
//! la agrupación por proyecto de compose.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::apps::DesktopApp;

/// Puertos de protocolos que no son HTTP: no se les manda un GET de prueba.
pub const NO_HTTP: &[u16] = &[
    21, 22, 23, 25, 49, 53, 110, 143, 389, 445, 636, 1433, 1521, 1883, 2049, 3306, 5432, 5672, 6379, 9042,
    10050, 10051, 11211, 27017,
];
/// Salidas normales: 0, o las de `docker stop` (SIGTERM y, si no hizo caso, SIGKILL).
pub const SALIDA_NORMAL: &[i64] = &[0, 137, 143];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Accent {
    Blue,
    Amber,
    Red,
    Green,
    Grey,
}

impl Accent {
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Accent::Blue => [0x3d, 0xae, 0xe9],
            Accent::Amber => [0xe8, 0xa3, 0x3d],
            Accent::Red => [0xda, 0x44, 0x53],
            Accent::Green => [0x3e, 0xc4, 0x6d],
            Accent::Grey => [0x7f, 0x8c, 0x8d],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Port {
    pub ip: String,
    pub host: u16,
    pub ctr: u16,
}

#[derive(Clone)]
pub struct Ctr {
    pub id: String,
    pub name: String,
    pub image: String,
    pub project: String,
    pub service: String,
    pub workdir: String,
    pub config_files: Vec<String>,
    /// running, exited, paused, restarting, created, dead
    pub state: String,
    /// healthy, unhealthy, starting o ""
    pub health: String,
    pub exit_code: i64,
    pub started: f64,
    pub finished: f64,
    pub restarts: i64,
    pub oom: bool,
    pub restart_policy: String,
    /// Solo TCP; 0.0.0.0 y :: cuentan como el mismo puerto
    pub ports: Vec<Port>,
    /// Puertos TCP que declara la imagen
    pub exposed: Vec<u16>,
    pub net_mode: String,
    /// red -> IP
    pub ips: Vec<(String, String)>,
    pub labels: BTreeMap<String, String>,
    pub cpu: Option<f64>,
    pub mem: Option<u64>,
    /// [(nombre, url)]
    pub webs: Vec<(String, String)>,
    /// Apps de escritorio, solo para contenedores sueltos
    pub apps: Vec<Arc<DesktopApp>>,
}

impl Ctr {
    pub fn alive(&self) -> bool {
        matches!(self.state.as_str(), "running" | "paused" | "restarting")
    }

    pub fn running(&self) -> bool {
        self.state == "running"
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    /// A partir de `GET /containers/{id}/json`.
    pub fn parse(info: &Value) -> Option<Ctr> {
        let s = |v: &Value| v.as_str().unwrap_or("").to_owned();
        let config = &info["Config"];
        let state = &info["State"];
        let net = &info["NetworkSettings"];
        let host_cfg = &info["HostConfig"];
        let labels: BTreeMap<String, String> = config["Labels"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), s(v))).collect())
            .unwrap_or_default();

        let mut ports: Vec<Port> = Vec::new();
        if let Some(map) = net["Ports"].as_object() {
            for (key, binds) in map {
                let (cport, proto) = key.split_once('/').unwrap_or((key.as_str(), "tcp"));
                let Ok(cport) = cport.parse::<u16>() else { continue };
                for bind in binds.as_array().into_iter().flatten() {
                    let Some(hport) = bind["HostPort"].as_str().and_then(|p| p.parse::<u16>().ok()) else {
                        continue;
                    };
                    if proto == "tcp" && !ports.iter().any(|p| p.host == hport) {
                        ports.push(Port { ip: s(&bind["HostIp"]), host: hport, ctr: cport });
                    }
                }
            }
        }
        let exposed = config["ExposedPorts"]
            .as_object()
            .map(|m| {
                m.keys()
                    .filter(|k| k.ends_with("/tcp"))
                    .filter_map(|k| k.split('/').next()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        let ips = net["Networks"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let ip = v["IPAddress"].as_str().unwrap_or("");
                        (!ip.is_empty()).then(|| (k.clone(), ip.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let get = |k: &str| labels.get(k).cloned().unwrap_or_default();
        Some(Ctr {
            id: info["Id"].as_str()?.to_owned(),
            name: s(&info["Name"]).trim_start_matches('/').to_owned(),
            image: s(&config["Image"]),
            project: get("com.docker.compose.project"),
            service: get("com.docker.compose.service"),
            workdir: get("com.docker.compose.project.working_dir"),
            config_files: get("com.docker.compose.project.config_files")
                .split(',')
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .collect(),
            state: s(&state["Status"]),
            health: s(&state["Health"]["Status"]),
            exit_code: state["ExitCode"].as_i64().unwrap_or(0),
            started: docker_time(state["StartedAt"].as_str().unwrap_or("")),
            finished: docker_time(state["FinishedAt"].as_str().unwrap_or("")),
            restarts: info["RestartCount"].as_i64().unwrap_or(0),
            oom: state["OOMKilled"].as_bool().unwrap_or(false),
            restart_policy: s(&host_cfg["RestartPolicy"]["Name"]),
            ports,
            exposed,
            net_mode: s(&host_cfg["NetworkMode"]),
            ips,
            labels,
            cpu: None,
            mem: None,
            webs: Vec::new(),
            apps: Vec::new(),
        })
    }
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Días desde 1970-01-01 (algoritmo de Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Marca de tiempo de Docker (RFC 3339 con nanosegundos) a epoch; 0 si nunca.
pub fn docker_time(v: &str) -> f64 {
    if v.is_empty() || v.starts_with("0001-") || v.len() < 19 {
        return 0.0;
    }
    let b = v.as_bytes();
    let num = |a: usize, z: usize| v.get(a..z).and_then(|s| s.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) =
        (num(0, 4), num(5, 7), num(8, 10), num(11, 13), num(14, 16), num(17, 19))
    else {
        return 0.0;
    };
    let mut i = 19;
    let mut frac = 0.0;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac = format!("0.{}", &v[start..i]).parse().unwrap_or(0.0);
    }
    let offset = match b.get(i) {
        Some(b'+') | Some(b'-') => {
            let sign = if b[i] == b'-' { -1 } else { 1 };
            let oh = num(i + 1, i + 3).unwrap_or(0);
            let om = num(i + 4, i + 6).unwrap_or(0);
            sign * (oh * 3600 + om * 60)
        }
        _ => 0,
    };
    let secs = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s - offset;
    secs as f64 + frac
}

pub fn ago(seconds: f64) -> String {
    let s = seconds.max(0.0) as i64;
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86400 {
        format!("{} h", s / 3600)
    } else if s < 172800 {
        "1 día".into()
    } else {
        format!("{} días", s / 86400)
    }
}

pub fn size(n: Option<u64>) -> String {
    match n {
        None => "--".into(),
        Some(n) if n >= 1 << 30 => format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64),
        Some(n) => format!("{:.0} MiB", n as f64 / (1u64 << 20) as f64),
    }
}

/// Color del punto y texto de estado, en castellano.
pub fn status_of(c: &Ctr) -> (Accent, String) {
    let t = now();
    match c.state.as_str() {
        "running" => {
            let text = format!("en marcha · {}", ago(t - c.started));
            match c.health.as_str() {
                "unhealthy" => (Accent::Amber, text + " · no sano"),
                "starting" => (Accent::Amber, text + " · arrancando"),
                "healthy" => (Accent::Green, text + " · sano"),
                _ => (Accent::Green, text),
            }
        }
        "paused" => (Accent::Amber, "en pausa".into()),
        "restarting" => (Accent::Red, format!("reiniciándose en bucle ({})", c.restarts)),
        "exited" => {
            let text = if c.finished > 0.0 {
                format!("parado hace {}", ago(t - c.finished))
            } else {
                "parado".into()
            };
            if c.oom {
                (Accent::Red, text + " · se quedó sin memoria")
            } else if !SALIDA_NORMAL.contains(&c.exit_code) {
                (Accent::Red, format!("{text} · código {}", c.exit_code))
            } else {
                (Accent::Grey, text)
            }
        }
        "created" => (Accent::Grey, "creado, sin arrancar".into()),
        "dead" => (Accent::Red, "muerto".into()),
        other => (Accent::Grey, other.into()),
    }
}

/// Estado de algo que va mal, sin el tiempo transcurrido: la bandeja no se
/// repinta sola (no sondea), así que un "hace 2 s" quedaría congelado.
pub fn trouble_text(c: &Ctr) -> String {
    match c.state.as_str() {
        "restarting" => format!("reiniciándose en bucle ({})", c.restarts),
        "dead" => "muerto".into(),
        _ => "en marcha · no sano".into(),
    }
}

pub fn is_trouble(c: &Ctr) -> bool {
    matches!(c.state.as_str(), "restarting" | "dead") || (c.running() && c.health == "unhealthy")
}

#[derive(Clone)]
pub struct Group {
    pub title: String,
    pub project: String,
    pub workdir: String,
    pub ctrs: Vec<Ctr>,
    pub apps: Vec<Arc<DesktopApp>>,
}

impl Group {
    pub fn key(&self) -> String {
        format!("p:{}", self.project)
    }
    pub fn running(&self) -> usize {
        self.ctrs.iter().filter(|c| c.running()).count()
    }
}

/// Proyectos por orden alfabético y los sueltos al final.
pub fn group_containers(ctrs: impl IntoIterator<Item = Ctr>) -> Vec<Group> {
    let mut map: BTreeMap<String, Group> = BTreeMap::new();
    for c in ctrs {
        let g = map.entry(c.project.clone()).or_insert_with(|| Group {
            title: if c.project.is_empty() { "Sueltos".into() } else { c.project.clone() },
            project: c.project.clone(),
            workdir: c.workdir.clone(),
            ctrs: Vec::new(),
            apps: Vec::new(),
        });
        g.ctrs.push(c);
    }
    let loose = map.remove("");
    let mut out: Vec<Group> = map.into_values().collect();
    out.extend(loose);
    for g in &mut out {
        g.ctrs.sort_by(|a, b| a.name.cmp(&b.name));
    }
    out
}

/// 'Nombre=url, url2' (o una lista JSON) -> [(nombre, url)].
pub fn parse_links(value: &Value) -> Vec<(String, String)> {
    let items: Vec<String> = match value {
        Value::Array(a) => {
            a.iter().map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).collect()
        }
        Value::String(s) => s.split(',').map(str::to_owned).collect(),
        Value::Null => Vec::new(),
        other => other.to_string().split(',').map(str::to_owned).collect(),
    };
    let mut links = Vec::new();
    for item in items {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (mut name, mut url) = match item.split_once('=') {
            Some((n, u)) if !n.contains("://") => (n.trim().to_owned(), u.trim().to_owned()),
            _ => (String::new(), item.to_owned()),
        };
        url = url.trim().to_owned();
        if name.is_empty() {
            name = netloc(&url).unwrap_or(&url).to_owned();
        }
        links.push((name, url));
    }
    links
}

fn netloc(url: &str) -> Option<&str> {
    let rest = match url.find("://") {
        Some(i) if url[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => &url[i + 3..],
        _ => url.strip_prefix("//")?,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiempos() {
        assert_eq!(docker_time("0001-01-01T00:00:00Z"), 0.0);
        assert_eq!(docker_time("1970-01-01T00:00:10Z"), 10.0);
        let t = docker_time("2026-11-22T15:45:12.123456789Z");
        assert!((t - 1795362312.123).abs() < 0.01, "{t}");
        assert_eq!(docker_time("2026-11-22T17:45:12+02:00"), docker_time("2026-11-22T15:45:12Z"));
    }

    #[test]
    fn enlaces() {
        let v = Value::String("Panel=http://localhost:8765, http://x:1/a".into());
        assert_eq!(
            parse_links(&v),
            vec![("Panel".into(), "http://localhost:8765".into()), ("x:1".into(), "http://x:1/a".into())]
        );
    }

    #[test]
    fn tamanos() {
        assert_eq!(size(Some(345 << 20)), "345 MiB");
        assert_eq!(size(Some(30 * (1 << 30) + (1 << 29))), "30.5 GiB");
        assert_eq!(ago(59.0), "59 s");
        assert_eq!(ago(90000.0), "1 día");
    }
}
