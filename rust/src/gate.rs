//! ¿Está dockerd en marcha? Se averigua SIN tocar su socket: con activación por
//! socket (docker.socket), conectarse a /var/run/docker.sock lo encendería.
//!
//! Lo normal es preguntarle a systemd por D-Bus el `ActiveState` de
//! docker.service y escuchar sus cambios (PropertiesChanged): cero sondeo. Sin
//! systemd o sin esa unidad se mira el fichero de PID de dockerd cada pocos
//! segundos.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Power {
    Unknown,
    Off,
    Starting,
    On,
    Stopping,
}

enum Source {
    Systemd { conn: Connection, unit: OwnedObjectPath },
    Pidfile(PathBuf),
    Socket(PathBuf),
}

type Listener = Box<dyn Fn(Power) + Send>;

pub struct Gate {
    state: Mutex<Power>,
    cv: Condvar,
    source: Source,
    listener: Mutex<Option<Listener>>,
}

const SYSTEMD: &str = "org.freedesktop.systemd1";

fn active_state_to_power(s: &str) -> Power {
    match s {
        "active" | "reloading" | "refreshing" => Power::On,
        "activating" => Power::Starting,
        "deactivating" => Power::Stopping,
        _ => Power::Off,
    }
}

fn value_str(v: &Value<'_>) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        Value::Value(inner) => value_str(inner),
        _ => None,
    }
}

fn unit_property(conn: &Connection, unit: &OwnedObjectPath, prop: &str) -> Option<String> {
    let reply = conn
        .call_method(
            Some(SYSTEMD),
            unit.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.systemd1.Unit", prop),
        )
        .ok()?;
    let v: OwnedValue = reply.body().deserialize().ok()?;
    value_str(&v)
}

fn systemd_source(user: bool) -> Option<Source> {
    let conn = if user { Connection::session() } else { Connection::system() }.ok()?;
    let reply = conn
        .call_method(
            Some(SYSTEMD),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "LoadUnit",
            &("docker.service",),
        )
        .ok()?;
    let unit: OwnedObjectPath = reply.body().deserialize().ok()?;
    if unit_property(&conn, &unit, "LoadState").as_deref() != Some("loaded") {
        return None;
    }
    Some(Source::Systemd { conn, unit })
}

/// PID vivo y que de verdad sea dockerd (un PID viejo puede haberse reciclado).
fn pidfile_alive(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let Ok(pid) = text.trim().parse::<u32>() else { return false };
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => comm.trim_end().starts_with("dockerd"),
        Err(_) => Path::new(&format!("/proc/{pid}")).exists(),
    }
}

impl Gate {
    pub fn new(sock: &Path) -> Arc<Gate> {
        let canon = std::fs::canonicalize(sock).unwrap_or_else(|_| sock.to_path_buf());
        let dir = canon.parent().map(Path::to_path_buf).unwrap_or_else(|| "/run".into());
        let runtime = crate::util::runtime_dir();
        let source = if canon == Path::new("/run/docker.sock") {
            systemd_source(false).unwrap_or(Source::Pidfile("/run/docker.pid".into()))
        } else if runtime.as_deref().is_some_and(|r| canon.starts_with(r)) {
            // Docker sin root: unidad de usuario y PID en $XDG_RUNTIME_DIR
            systemd_source(true).unwrap_or(Source::Pidfile(dir.join("docker.pid")))
        } else if dir.join("docker.pid").exists() {
            Source::Pidfile(dir.join("docker.pid"))
        } else {
            Source::Socket(canon)
        };
        let gate = Arc::new(Gate {
            state: Mutex::new(Power::Unknown),
            cv: Condvar::new(),
            source,
            listener: Mutex::new(None),
        });
        gate.refresh();
        gate
    }

    fn query(&self) -> Power {
        match &self.source {
            Source::Systemd { conn, unit } => match unit_property(conn, unit, "ActiveState") {
                Some(s) => active_state_to_power(&s),
                None => Power::Unknown,
            },
            Source::Pidfile(p) => {
                if pidfile_alive(p) {
                    Power::On
                } else {
                    Power::Off
                }
            }
            Source::Socket(p) => {
                if p.exists() {
                    Power::On
                } else {
                    Power::Off
                }
            }
        }
    }

    fn set(&self, p: Power) -> bool {
        let mut st = self.state.lock().unwrap();
        let changed = *st != p;
        *st = p;
        drop(st);
        if changed {
            self.cv.notify_all();
            if let Some(cb) = self.listener.lock().unwrap().as_ref() {
                cb(p);
            }
        }
        changed
    }

    pub fn get(&self) -> Power {
        *self.state.lock().unwrap()
    }

    pub fn is_on(&self) -> bool {
        self.get() == Power::On
    }

    /// Pregunta de nuevo (sin fiarse de lo último que se supo).
    pub fn refresh(&self) -> Power {
        let p = self.query();
        self.set(p);
        p
    }

    pub fn wait_until_on(&self) {
        let mut st = self.state.lock().unwrap();
        while *st != Power::On {
            st = self.cv.wait(st).unwrap();
        }
    }

    /// Hilo que sigue los cambios; `on_change` se llama con cada estado nuevo,
    /// lo detecte quien lo detecte.
    pub fn watch(self: &Arc<Self>, on_change: impl Fn(Power) + Send + 'static) {
        *self.listener.lock().unwrap() = Some(Box::new(on_change));
        let gate = Arc::clone(self);
        std::thread::Builder::new()
            .name("estado-docker".into())
            .stack_size(256 * 1024)
            .spawn(move || gate.watch_loop())
            .expect("no se pudo crear el hilo de estado de Docker");
    }

    fn watch_loop(&self) {
        if let Source::Systemd { conn, unit } = &self.source {
            // systemd solo emite señales si algún cliente se ha suscrito
            let _ = conn.call_method(
                Some(SYSTEMD),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "Subscribe",
                &(),
            );
            let rule = format!(
                "type='signal',sender='{SYSTEMD}',path='{}',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged'",
                unit.as_str()
            );
            if let Ok(iter) = MessageIterator::for_match_rule(rule.as_str(), conn, Some(64)) {
                // lo que pudo cambiar entre la consulta inicial y la suscripción
                self.refresh();
                for msg in iter {
                    let Ok(msg) = msg else { continue };
                    let body = msg.body();
                    let Ok((iface, changed, _inv)) =
                        body.deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                    else {
                        continue;
                    };
                    if iface != "org.freedesktop.systemd1.Unit" {
                        continue;
                    }
                    if let Some(s) = changed.get("ActiveState").and_then(|v| value_str(v)) {
                        self.set(active_state_to_power(&s));
                    }
                }
            }
            // se perdió el bus: se sigue con un sondeo lento
        }
        loop {
            std::thread::sleep(Duration::from_secs(3));
            self.refresh();
        }
    }
}
