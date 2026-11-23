//! Motor común a la bandeja y a la ventana. Dirigido por eventos: una lista
//! inicial y luego el flujo /events de Docker; cada evento reinspecciona solo
//! ese contenedor. Nada de sondear: la CPU y la RAM (stats) solo se piden si
//! se activan, y eso lo hace únicamente la ventana mientras está abierta.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::apps::{self, DesktopApp};
use crate::config::Links;
use crate::docker::Docker;
use crate::gate::{Gate, Power};
use crate::model::{self, Ctr, Group, NO_HTTP, group_containers, is_trouble, parse_links, trouble_text};
use crate::probe::probe_web;
use crate::util::urlencode;

const APPS_RESCAN: Duration = Duration::from_secs(30);
/// Un kill/stop reciente convierte el "die" en un apagado normal.
const INTENCIONADO: Duration = Duration::from_secs(15);
const DEBOUNCE: Duration = Duration::from_millis(120);
const STATS_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    Start,
    Stop,
    Restart,
    Pause,
    Unpause,
}

impl Verb {
    pub fn api(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
            Verb::Pause => "pause",
            Verb::Unpause => "unpause",
        }
    }
    pub fn busy_text(self) -> &'static str {
        match self {
            Verb::Start => "Encendiendo…",
            Verb::Stop => "Apagando…",
            Verb::Restart => "Reiniciando…",
            Verb::Pause => "Pausando…",
            Verb::Unpause => "Reanudando…",
        }
    }
}

type ProbeKey = (String, u64, u16);

pub struct Sample {
    id: String,
    total: Option<u64>,
    system: Option<u64>,
    online: Option<u64>,
    mem: Option<u64>,
}

pub enum Msg {
    Power,
    StreamUp,
    StreamDown(Option<String>),
    Event(Value),
    Probes(Vec<(ProbeKey, Option<String>, bool)>),
    Stats(Vec<Sample>),
    Done { key: String, err: Option<String> },
    Container { id: String, verb: Verb },
    Project { project: String, verb: Verb },
    WakeDocker,
    MenuOpening,
}

/// Lo que ven la bandeja y la ventana.
pub struct Snapshot {
    pub power: Power,
    /// Hay flujo de eventos y la lista inicial se leyó bien
    pub connected: bool,
    pub error: Option<String>,
    pub groups: Vec<Group>,
    pub total: usize,
    pub running: usize,
    /// (nombre, estado) de los que van mal
    pub trouble: Vec<(String, String)>,
    pub busy: BTreeMap<String, &'static str>,
    pub waking: bool,
    pub ncpu: usize,
    pub mem_total: u64,
    pub config_error: String,
}

impl Snapshot {
    pub fn ctrs(&self) -> impl Iterator<Item = &Ctr> {
        self.groups.iter().flat_map(|g| g.ctrs.iter())
    }
}

pub type NoticeFn = Box<dyn Fn(&str, &str) + Send>;

pub struct Hooks {
    pub publish: Box<dyn Fn(Arc<Snapshot>) + Send>,
    /// Avisos de caídas (solo la bandeja)
    pub notice: Option<NoticeFn>,
    /// Errores de acciones para enseñar al usuario
    pub error: Box<dyn Fn(String) + Send>,
}

struct ProbeState {
    url: Option<String>,
    definitive: bool,
    checked: Instant,
}

#[derive(Default)]
struct Notices {
    stopped_at: HashMap<String, Instant>,
    oom_at: HashMap<String, Instant>,
    health: HashMap<String, String>,
}

struct Engine {
    docker: Arc<Docker>,
    gate: Arc<Gate>,
    tx: Sender<Msg>,
    hooks: Hooks,
    stats: bool,
    power: Power,
    connected: bool,
    error: Option<String>,
    ctrs: HashMap<String, Ctr>,
    busy: BTreeMap<String, &'static str>,
    waking: bool,
    probes: HashMap<ProbeKey, ProbeState>,
    in_flight: HashSet<ProbeKey>,
    probe_wake: Option<Instant>,
    apps: Vec<Arc<DesktopApp>>,
    apps_at: Option<Instant>,
    links: Links,
    prev_cpu: HashMap<String, (Option<u64>, Option<u64>)>,
    stats_next: Option<Instant>,
    stats_in_flight: bool,
    dirty_ids: HashSet<String>,
    flush_at: Option<Instant>,
    full_at: Option<Instant>,
    notices: Notices,
    ncpu: usize,
    mem_total: u64,
    dirty: bool,
}

fn mem_total() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn ncpu() -> usize {
    std::fs::read_to_string("/sys/devices/system/cpu/online")
        .ok()
        .and_then(|s| {
            let mut n = 0;
            for part in s.trim().split(',') {
                match part.split_once('-') {
                    Some((a, b)) => n += b.parse::<usize>().ok()? - a.parse::<usize>().ok()? + 1,
                    None => n += 1,
                }
            }
            Some(n)
        })
        .or_else(|| thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(1)
}

fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = thread::Builder::new().name(name.into()).stack_size(512 * 1024).spawn(f);
}

/// Arranca el motor en su hilo y devuelve el canal para mandarle órdenes.
pub fn start(stats: bool, hooks: Hooks) -> Sender<Msg> {
    let sock = Docker::socket_from_env();
    let gate = Gate::new(&sock);
    let docker = Arc::new(Docker::new(sock, Arc::clone(&gate)));
    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        gate.watch(move |_| {
            let _ = tx.send(Msg::Power);
        });
    }
    {
        let (docker, gate, tx) = (Arc::clone(&docker), Arc::clone(&gate), tx.clone());
        spawn_named("eventos", move || events_loop(docker, gate, tx));
    }
    let engine = Engine {
        power: gate.get(),
        docker,
        gate,
        tx: tx.clone(),
        hooks,
        stats,
        connected: false,
        error: None,
        ctrs: HashMap::new(),
        busy: BTreeMap::new(),
        waking: false,
        probes: HashMap::new(),
        in_flight: HashSet::new(),
        probe_wake: None,
        apps: Vec::new(),
        apps_at: None,
        links: Links::default(),
        prev_cpu: HashMap::new(),
        stats_next: None,
        stats_in_flight: false,
        dirty_ids: HashSet::new(),
        flush_at: None,
        full_at: None,
        notices: Notices::default(),
        ncpu: ncpu(),
        mem_total: mem_total(),
        dirty: true,
    };
    spawn_named("motor", move || engine.run(rx));
    tx
}

const EVENTS: &str = r#"{"type":["container"],"event":["create","start","restart","die","stop","kill","oom","pause","unpause","destroy","rename","update","health_status"]}"#;

/// Flujo /events. Solo se conecta con dockerd en marcha (confirmado justo
/// antes); si el flujo se corta, reintenta con pausas crecientes.
fn events_loop(docker: Arc<Docker>, gate: Arc<Gate>, tx: Sender<Msg>) {
    let url = format!("/events?filters={}", urlencode(EVENTS));
    let mut pause = 3;
    loop {
        gate.wait_until_on();
        if gate.refresh() != Power::On {
            continue;
        }
        match docker.stream(&url) {
            Ok(mut reader) => {
                pause = 3;
                if tx.send(Msg::StreamUp).is_err() {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(v) = serde_json::from_str::<Value>(&line)
                                && tx.send(Msg::Event(v)).is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                if tx.send(Msg::StreamDown(None)).is_err() {
                    return;
                }
            }
            Err(e) => {
                if tx.send(Msg::StreamDown(Some(e))).is_err() {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_secs(pause));
        pause = (pause * 2).min(60);
    }
}

fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

impl Engine {
    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            if self.dirty {
                self.publish();
                self.dirty = false;
            }
            let deadline = [self.flush_at, self.full_at, self.probe_wake, self.stats_next]
                .into_iter()
                .fold(None, earliest);
            let first = match deadline {
                Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                },
            };
            if let Some(m) = first {
                self.handle(m);
            }
            while let Ok(m) = rx.try_recv() {
                self.handle(m);
            }
            self.tick();
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Power => {
                let p = self.gate.get();
                if p != self.power {
                    self.power = p;
                    if p != Power::On {
                        self.connected = false;
                        self.ctrs.clear();
                        self.prev_cpu.clear();
                        self.dirty_ids.clear();
                        self.flush_at = None;
                        self.full_at = None;
                        self.stats_next = None;
                        self.error = None;
                    }
                    self.dirty = true;
                }
            }
            Msg::StreamUp => {
                self.full_refresh();
                if self.connected && self.stats {
                    self.stats_next = Some(Instant::now());
                }
            }
            Msg::StreamDown(err) => {
                self.connected = false;
                if self.gate.is_on() {
                    self.error = err;
                }
                self.dirty = true;
            }
            Msg::Event(ev) => self.on_event(ev),
            Msg::Probes(results) => {
                let now = Instant::now();
                for (key, url, definitive) in results {
                    self.in_flight.remove(&key);
                    self.probes.insert(key, ProbeState { url, definitive, checked: now });
                }
                self.dirty = true;
            }
            Msg::Stats(samples) => {
                self.stats_in_flight = false;
                self.merge_stats(samples);
                self.dirty = true;
            }
            Msg::Done { key, err } => {
                if key == "docker" {
                    self.waking = false;
                    if let Some(e) = err {
                        (self.hooks.error)(format!("No se pudo encender Docker: {e}"));
                    }
                } else {
                    self.busy.remove(&key);
                    if let Some(e) = err {
                        let name = match key.strip_prefix("p:") {
                            Some(p) => p.to_owned(),
                            None => self
                                .ctrs
                                .get(&key)
                                .map(|c| c.name.clone())
                                .unwrap_or_else(|| key.chars().take(12).collect()),
                        };
                        (self.hooks.error)(format!("{name}: {e}"));
                    }
                    self.full_at = Some(Instant::now());
                }
                self.dirty = true;
            }
            Msg::Container { id, verb } => self.container_action(id, verb),
            Msg::Project { project, verb } => self.project_action(project, verb),
            Msg::WakeDocker => {
                if !self.waking {
                    self.waking = true;
                    self.dirty = true;
                    let (docker, gate, tx) =
                        (Arc::clone(&self.docker), Arc::clone(&self.gate), self.tx.clone());
                    spawn_named("encender", move || {
                        let err = docker.ping().err();
                        gate.refresh();
                        let _ = tx.send(Msg::Done { key: "docker".into(), err });
                    });
                }
            }
            Msg::MenuOpening => {
                let old = self.apps_at;
                self.maybe_rescan_apps();
                if self.apps_at != old || self.links.reload() {
                    self.dirty = true;
                }
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.full_at.is_some_and(|t| t <= now) {
            self.full_at = None;
            if self.gate.is_on() && self.connected {
                self.full_refresh();
            }
        }
        if self.flush_at.is_some_and(|t| t <= now) {
            self.flush_at = None;
            let ids: Vec<String> = self.dirty_ids.drain().collect();
            for id in ids {
                match self.inspect(&id) {
                    Ok(Some(c)) => self.insert(c),
                    Ok(None) => {
                        self.ctrs.remove(&id);
                        self.prev_cpu.remove(&id);
                    }
                    Err(_) => {}
                }
            }
            self.dirty = true;
        }
        if self.probe_wake.is_some_and(|t| t <= now) {
            self.probe_wake = None;
            self.dirty = true;
        }
        if self.stats_next.is_some_and(|t| t <= now) {
            // la primera lectura solo deja la referencia: la segunda va antes
            let first = self.prev_cpu.is_empty();
            self.stats_next = Some(now + if first { Duration::from_secs(1) } else { STATS_EVERY });
            if self.stats && self.connected && !self.stats_in_flight {
                self.start_stats();
            }
        }
    }

    fn inspect(&self, id: &str) -> Result<Option<Ctr>, String> {
        match self.docker.get(&format!("/containers/{id}/json")) {
            Ok(v) => Ok(Ctr::parse(&v)),
            Err(e) if e.contains("No such container") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Conserva la última CPU/RAM leída mientras siga en marcha.
    fn insert(&mut self, mut c: Ctr) {
        if c.running() {
            if let Some(old) = self.ctrs.get(&c.id) {
                c.cpu = old.cpu;
                c.mem = old.mem;
            }
        } else {
            self.prev_cpu.remove(&c.id);
        }
        self.ctrs.insert(c.id.clone(), c);
    }

    fn full_refresh(&mut self) {
        let rows = match self.docker.get("/containers/json?all=1") {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(e);
                self.connected = false;
                self.dirty = true;
                return;
            }
        };
        let ids: Vec<String> = rows
            .as_array()
            .map(|a| a.iter().filter_map(|r| r["Id"].as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let mut old = std::mem::take(&mut self.ctrs);
        for id in &ids {
            if let Ok(Some(mut c)) = self.inspect(id) {
                if c.running()
                    && let Some(o) = old.remove(&c.id)
                {
                    c.cpu = o.cpu;
                    c.mem = o.mem;
                }
                self.ctrs.insert(c.id.clone(), c);
            }
        }
        self.prev_cpu.retain(|id, _| self.ctrs.get(id).is_some_and(|c| c.running()));
        self.connected = true;
        self.error = None;
        self.dirty = true;
    }

    fn on_event(&mut self, ev: Value) {
        let action = ev["Action"].as_str().unwrap_or("").to_owned();
        let kind = action.split(':').next().unwrap_or("").to_owned();
        let actor = &ev["Actor"];
        let cid = actor["ID"].as_str().unwrap_or("").to_owned();
        if cid.is_empty() {
            return;
        }
        let attrs = &actor["Attributes"];
        let name = attrs["name"].as_str().unwrap_or("?");
        let now = Instant::now();
        let n = &mut self.notices;
        let mut notice: Option<(String, String)> = None;
        match kind.as_str() {
            "kill" | "stop" => {
                n.stopped_at.insert(cid.clone(), now);
            }
            "oom" => {
                n.oom_at.insert(cid.clone(), now);
                notice =
                    Some((format!("«{name}» se quedó sin memoria"), "El kernel lo ha matado (OOM).".into()));
            }
            "die" => {
                let code = attrs["exitCode"].as_str().unwrap_or("0");
                let on_purpose = n.stopped_at.get(&cid).is_some_and(|t| now - *t < INTENCIONADO);
                let after_oom = n.oom_at.get(&cid).is_some_and(|t| now - *t <= Duration::from_secs(5));
                if code != "0" && !on_purpose && !after_oom {
                    notice = Some((
                        format!("Se ha caído «{name}»"),
                        format!("El contenedor terminó con código {code}."),
                    ));
                }
            }
            "health_status" => {
                let status = action.split_once(':').map(|x| x.1.trim()).unwrap_or("").to_owned();
                if status == "unhealthy" && n.health.get(&cid).map(String::as_str) != Some("unhealthy") {
                    notice = Some((format!("«{name}» no está sano"), "Su healthcheck está fallando.".into()));
                }
                n.health.insert(cid.clone(), status);
            }
            "destroy" => {
                n.stopped_at.remove(&cid);
                n.oom_at.remove(&cid);
                n.health.remove(&cid);
            }
            _ => {}
        }
        if let (Some((title, body)), Some(cb)) = (notice, self.hooks.notice.as_ref()) {
            cb(&title, &body);
        }
        self.dirty_ids.insert(cid);
        if self.flush_at.is_none() {
            self.flush_at = Some(now + DEBOUNCE);
        }
    }

    fn container_action(&mut self, id: String, verb: Verb) {
        if self.busy.contains_key(&id) {
            return;
        }
        self.busy.insert(id.clone(), verb.busy_text());
        self.dirty = true;
        let (docker, tx) = (Arc::clone(&self.docker), self.tx.clone());
        spawn_named("accion", move || {
            let q = if matches!(verb, Verb::Stop | Verb::Restart) { "?t=10" } else { "" };
            let err = docker
                .call("POST", &format!("/containers/{id}/{}{q}", verb.api()), Duration::from_secs(90))
                .err();
            let _ = tx.send(Msg::Done { key: id, err });
        });
    }

    /// Con compose si sus ficheros siguen ahí (respeta depends_on); si no, uno a uno.
    fn project_action(&mut self, project: String, verb: Verb) {
        let key = format!("p:{project}");
        if project.is_empty() || self.busy.contains_key(&key) {
            return;
        }
        let members: Vec<&Ctr> = self.ctrs.values().filter(|c| c.project == project).collect();
        let Some(first) = members.first() else { return };
        let workdir = first.workdir.clone();
        let files: Vec<String> =
            first.config_files.iter().filter(|f| std::path::Path::new(f).exists()).cloned().collect();
        let targets: Vec<(String, bool)> = members.iter().map(|c| (c.id.clone(), c.alive())).collect();
        self.busy.insert(key.clone(), verb.busy_text());
        self.dirty = true;
        let (docker, tx) = (Arc::clone(&self.docker), self.tx.clone());
        spawn_named("accion", move || {
            let err = (|| -> Result<(), String> {
                if !files.is_empty() && crate::util::which("docker").is_some() {
                    let mut cmd = std::process::Command::new("docker");
                    cmd.args(["compose", "-p", &project, "--project-directory", &workdir]);
                    for f in &files {
                        cmd.args(["-f", f]);
                    }
                    let out = cmd
                        .arg(verb.api())
                        .stdin(std::process::Stdio::null())
                        .output()
                        .map_err(|e| e.to_string())?;
                    if !out.status.success() {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let last = stderr.lines().rfind(|l| !l.trim().is_empty());
                        return Err(last
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("docker compose {} falló", verb.api())));
                    }
                    return Ok(());
                }
                let q = if matches!(verb, Verb::Stop | Verb::Restart) { "?t=10" } else { "" };
                for (id, alive) in targets {
                    if (verb == Verb::Start) != alive {
                        docker.call(
                            "POST",
                            &format!("/containers/{id}/{}{q}", verb.api()),
                            Duration::from_secs(90),
                        )?;
                    }
                }
                Ok(())
            })()
            .err();
            let _ = tx.send(Msg::Done { key, err });
        });
    }

    fn start_stats(&mut self) {
        let ids: Vec<String> = self.ctrs.values().filter(|c| c.running()).map(|c| c.id.clone()).collect();
        if ids.is_empty() {
            return;
        }
        self.stats_in_flight = true;
        let (docker, tx) = (Arc::clone(&self.docker), self.tx.clone());
        spawn_named("stats", move || {
            let per = ids.len().div_ceil(4);
            let samples: Vec<Sample> = thread::scope(|s| {
                let handles: Vec<_> = ids
                    .chunks(per)
                    .map(|chunk| {
                        let docker = &docker;
                        s.spawn(move || {
                            chunk.iter().filter_map(|id| read_stats(docker, id)).collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
            });
            let _ = tx.send(Msg::Stats(samples));
        });
    }

    fn merge_stats(&mut self, samples: Vec<Sample>) {
        for s in samples {
            let prev = self.prev_cpu.insert(s.id.clone(), (s.total, s.system));
            let Some(c) = self.ctrs.get_mut(&s.id) else { continue };
            if !c.running() {
                continue;
            }
            if let (Some(t), Some(sy), Some((Some(pt), Some(ps)))) = (s.total, s.system, prev)
                && sy > ps
            {
                let cores = s.online.filter(|&n| n > 0).unwrap_or(self.ncpu as u64) as f64;
                let cpu = (t as f64 - pt as f64) / (sy - ps) as f64 * cores * 100.0;
                c.cpu = Some(cpu.max(0.0));
            }
            if s.mem.is_some() {
                c.mem = s.mem;
            }
        }
    }

    fn maybe_rescan_apps(&mut self) {
        if self.apps_at.is_none_or(|t| t.elapsed() > APPS_RESCAN) {
            self.apps = apps::scan_apps();
            self.apps_at = Some(Instant::now());
        }
    }

    fn webs_for(
        &mut self,
        c: &Ctr,
        published: &HashSet<u16>,
        pending: &mut Vec<(ProbeKey, String, u16)>,
    ) -> Vec<(String, String)> {
        let mut explicit = parse_links(&Value::String(c.label("zendock.web").unwrap_or("").into()));
        explicit.extend(parse_links(self.links.entry(&c.name, "web")));
        if !explicit.is_empty() {
            return explicit;
        }
        if !c.running() {
            return Vec::new();
        }
        let mut candidates: Vec<(String, u16)> = Vec::new();
        for p in &c.ports {
            if !NO_HTTP.contains(&p.host) && !NO_HTTP.contains(&p.ctr) {
                let host = if matches!(p.ip.as_str(), "" | "0.0.0.0" | "::") {
                    "127.0.0.1".to_owned()
                } else {
                    p.ip.clone()
                };
                candidates.push((host, p.host));
            }
        }
        if c.net_mode == "host" {
            candidates.extend(
                c.exposed
                    .iter()
                    .filter(|p| !NO_HTTP.contains(p) && !published.contains(p))
                    .map(|&p| ("127.0.0.1".to_owned(), p)),
            );
        }
        // Sin respuesta se reintenta: cada 3 s en los dos primeros minutos, luego cada minuto
        let retry =
            if model::now() - c.started < 120.0 { Duration::from_secs(3) } else { Duration::from_secs(60) };
        let now = Instant::now();
        let mut webs = Vec::new();
        for (host, port) in candidates {
            let key: ProbeKey = (c.id.clone(), c.started.to_bits(), port);
            let st = self.probes.get(&key);
            let definitive = st.is_some_and(|s| s.definitive);
            if !definitive && !self.in_flight.contains(&key) {
                match st.map(|s| s.checked + retry) {
                    Some(due) if due > now => self.probe_wake = earliest(self.probe_wake, Some(due)),
                    _ => pending.push((key.clone(), host, port)),
                }
            }
            if let Some(url) = self.probes.get(&key).and_then(|s| s.url.clone()) {
                webs.push((format!("Web :{port}"), url));
            }
        }
        webs
    }

    fn spawn_probes(&mut self, pending: Vec<(ProbeKey, String, u16)>) {
        if pending.is_empty() {
            return;
        }
        for (k, _, _) in &pending {
            self.in_flight.insert(k.clone());
        }
        let tx = self.tx.clone();
        spawn_named("sondas", move || {
            let results = thread::scope(|s| {
                let handles: Vec<_> = pending
                    .into_iter()
                    .map(|(key, host, port)| {
                        s.spawn(move || {
                            let (url, definitive) = probe_web(&host, port);
                            (key, url, definitive)
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            let _ = tx.send(Msg::Probes(results));
        });
    }

    fn publish(&mut self) {
        self.links.reload();
        if self.connected {
            self.maybe_rescan_apps();
        }
        self.probe_wake = None;
        let published: HashSet<u16> =
            self.ctrs.values().filter(|c| c.alive()).flat_map(|c| c.ports.iter().map(|p| p.host)).collect();
        let mut list: Vec<Ctr> = self.ctrs.values().cloned().collect();
        let mut pending = Vec::new();
        for c in &mut list {
            c.webs = self.webs_for(c, &published, &mut pending);
            if c.project.is_empty() {
                c.apps = apps::match_apps(
                    &self.apps,
                    &c.name,
                    "",
                    &self.links,
                    c.label("zendock.app").unwrap_or(""),
                );
            }
        }
        self.spawn_probes(pending);
        let mut groups = group_containers(list);
        for g in &mut groups {
            if !g.project.is_empty() {
                let label =
                    g.ctrs.iter().filter_map(|c| c.label("zendock.app")).collect::<Vec<_>>().join(",");
                g.apps = apps::match_apps(&self.apps, &g.project, &g.workdir, &self.links, &label);
            }
        }
        let all = || groups.iter().flat_map(|g| g.ctrs.iter());
        let snap = Snapshot {
            power: self.power,
            connected: self.connected,
            error: self.error.clone(),
            total: all().count(),
            running: all().filter(|c| c.running()).count(),
            trouble: all().filter(|c| is_trouble(c)).map(|c| (c.name.clone(), trouble_text(c))).collect(),
            groups,
            busy: self.busy.clone(),
            waking: self.waking,
            ncpu: self.ncpu,
            mem_total: self.mem_total,
            config_error: self.links.error.clone(),
        };
        (self.hooks.publish)(Arc::new(snap));
    }
}

fn read_stats(docker: &Docker, id: &str) -> Option<Sample> {
    let s = docker
        .call("GET", &format!("/containers/{id}/stats?stream=false&one-shot=true"), Duration::from_secs(5))
        .ok()??;
    let cpu = &s["cpu_stats"];
    let mem = &s["memory_stats"];
    let usage = mem["usage"].as_u64();
    let extra = &mem["stats"];
    let inactive =
        extra["inactive_file"].as_u64().or_else(|| extra["total_inactive_file"].as_u64()).unwrap_or(0);
    Some(Sample {
        id: id.to_owned(),
        total: cpu["cpu_usage"]["total_usage"].as_u64(),
        system: cpu["system_cpu_usage"].as_u64(),
        online: cpu["online_cpus"].as_u64(),
        mem: usage.map(|u| u.saturating_sub(inactive)),
    })
}
