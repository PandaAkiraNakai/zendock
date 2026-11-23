//! `zendock --tray`: el demonio de la bandeja, sin toolkit gráfico. Icono y
//! menú por StatusNotifierItem (D-Bus), avisos por org.freedesktop.Notifications
//! y los datos del motor común, que solo despierta con eventos de Docker.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};

use ksni::MenuItem;
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};

use crate::apps::{self, DesktopApp};
use crate::config::Settings;
use crate::engine::{self, Hooks, Msg, Snapshot, Verb};
use crate::gate::Power;
use crate::model::Accent;
use crate::{icon, notify, util};

struct ZenTray {
    tx: Sender<Msg>,
    snap: Option<Arc<Snapshot>>,
    avisos: Arc<AtomicBool>,
    png_blue: Vec<u8>,
    png_grey: Vec<u8>,
}

/// DBusMenu toma "_" como atajo de teclado: hay que doblarlo.
fn esc(s: &str) -> String {
    s.replace('_', "__")
}

fn item(label: String, enabled: bool, f: impl Fn(&mut ZenTray) + Send + 'static) -> MenuItem<ZenTray> {
    StandardItem { label: esc(&label), enabled, activate: Box::new(f), ..Default::default() }.into()
}

pub fn launch_window() {
    let exe = std::env::current_exe().unwrap_or_else(|_| "zendock".into());
    let exe = exe.to_string_lossy().trim_end_matches(" (deleted)").to_owned();
    let mut envs = Vec::new();
    if let Some(token) = ksni::current_xdg_activation_token() {
        envs.push(("XDG_ACTIVATION_TOKEN", token));
    }
    util::spawn_detached(&exe, std::iter::empty::<&str>(), &envs);
}

fn app_item(app: &Arc<DesktopApp>) -> MenuItem<ZenTray> {
    let a = Arc::clone(app);
    let mut it = StandardItem {
        label: esc(&format!("Abrir {}", app.name)),
        activate: Box::new(move |_: &mut ZenTray| {
            apps::launch_app(&a);
        }),
        ..Default::default()
    };
    if std::path::Path::new(&app.icon).is_absolute() {
        if let Some(p) = apps::icon_png_path(&app.icon) {
            it.icon_data = std::fs::read(p).unwrap_or_default();
        }
    } else {
        it.icon_name = app.icon.clone();
    }
    it.into()
}

impl ZenTray {
    fn accent(&self) -> Accent {
        let Some(s) = &self.snap else { return Accent::Grey };
        if s.waking || s.power == Power::Starting {
            return Accent::Amber;
        }
        if s.power != Power::On || !s.connected {
            return Accent::Grey;
        }
        if !s.trouble.is_empty() || !s.busy.is_empty() {
            Accent::Amber
        } else if s.running > 0 {
            Accent::Blue
        } else {
            Accent::Grey
        }
    }

    fn tip(&self) -> String {
        let Some(s) = &self.snap else { return "ZenDock\nComprobando Docker…".into() };
        let body = match s.power {
            Power::Off if s.waking => "Encendiendo Docker…".to_owned(),
            Power::Off => "Docker apagado".to_owned(),
            Power::Starting => "Encendiendo Docker…".to_owned(),
            Power::Stopping => "Docker se está apagando…".to_owned(),
            Power::Unknown => "Comprobando Docker…".to_owned(),
            Power::On if !s.connected => s.error.clone().unwrap_or("Conectando con Docker…".into()),
            Power::On => {
                let mut t = format!("{} de {} en marcha", s.running, s.total);
                for (name, status) in &s.trouble {
                    t.push_str(&format!("\n⚠ {name}: {status}"));
                }
                t
            }
        };
        format!("ZenDock\n{body}")
    }

    fn docker_menu(&self, s: &Snapshot) -> Vec<MenuItem<Self>> {
        let mut m = Vec::new();
        for g in &s.groups {
            let running = g.running();
            let mut sub: Vec<MenuItem<Self>> = Vec::new();
            if !g.project.is_empty() {
                let busy = s.busy.contains_key(&g.key());
                let any_off = g.ctrs.iter().any(|c| !c.alive());
                let any_on = g.ctrs.iter().any(|c| c.alive());
                for (verb, label, enabled) in [
                    (Verb::Start, "Encender todo", any_off),
                    (Verb::Stop, "Apagar todo", any_on),
                    (Verb::Restart, "Reiniciar todo", running > 0),
                ] {
                    let project = g.project.clone();
                    sub.push(item(label.into(), enabled && !busy, move |t| {
                        let _ = t.tx.send(Msg::Project { project: project.clone(), verb });
                    }));
                }
            } else {
                for c in &g.ctrs {
                    let alive = c.alive();
                    let id = c.id.clone();
                    let verb = if alive { Verb::Stop } else { Verb::Start };
                    let label = format!("{} {}", if alive { "Apagar" } else { "Encender" }, c.name);
                    sub.push(item(label, !s.busy.contains_key(&c.id), move |t| {
                        let _ = t.tx.send(Msg::Container { id: id.clone(), verb });
                    }));
                }
            }
            let apps: Vec<&Arc<DesktopApp>> =
                g.apps.iter().chain(g.ctrs.iter().flat_map(|c| c.apps.iter())).collect();
            let webs: Vec<(String, String, String)> = g
                .ctrs
                .iter()
                .filter(|c| c.running())
                .flat_map(|c| c.webs.iter().map(move |(n, u)| (c.name.clone(), n.clone(), u.clone())))
                .collect();
            if !apps.is_empty() || !webs.is_empty() {
                sub.push(MenuItem::Separator);
            }
            for a in apps {
                sub.push(app_item(a));
            }
            for (cname, name, url) in webs {
                sub.push(
                    StandardItem {
                        label: esc(&format!("{cname} · {name}")),
                        icon_name: "internet-web-browser".into(),
                        activate: Box::new(move |_: &mut Self| {
                            util::open_url(&url);
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            m.push(
                SubMenu {
                    label: esc(&format!("{}   {}/{}", g.title, running, g.ctrs.len())),
                    icon_data: if running > 0 { self.png_blue.clone() } else { self.png_grey.clone() },
                    submenu: sub,
                    ..Default::default()
                }
                .into(),
            );
        }
        m
    }
}

impl ksni::Tray for ZenTray {
    fn id(&self) -> String {
        "zendock".into()
    }
    fn title(&self) -> String {
        "ZenDock".into()
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::SystemServices
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let accent = self.accent();
        [22, 32, 48, 64]
            .into_iter()
            .map(|s| ksni::Icon { width: s, height: s, data: icon::argb(accent, s as u32) })
            .collect()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.tip(), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        launch_window();
    }
    fn menu_about_to_show(&mut self) {
        let _ = self.tx.send(Msg::MenuOpening);
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut m = Vec::new();
        if let Some(s) = self.snap.clone() {
            match s.power {
                Power::On if s.connected => {
                    m.extend(self.docker_menu(&s));
                    m.push(MenuItem::Separator);
                }
                Power::On => {
                    let text = s.error.clone().unwrap_or("Conectando con Docker…".into());
                    m.push(item(text, false, |_| {}));
                    m.push(MenuItem::Separator);
                }
                Power::Stopping => {
                    m.push(item("Docker se está apagando…".into(), false, |_| {}));
                    m.push(MenuItem::Separator);
                }
                _ => {
                    let waking = s.waking || s.power == Power::Starting;
                    let label = if waking { "Encendiendo Docker…" } else { "Encender Docker" };
                    m.push(
                        StandardItem {
                            label: label.into(),
                            enabled: !waking,
                            icon_name: "media-playback-start".into(),
                            activate: Box::new(|t: &mut Self| {
                                let _ = t.tx.send(Msg::WakeDocker);
                            }),
                            ..Default::default()
                        }
                        .into(),
                    );
                    m.push(MenuItem::Separator);
                }
            }
        }
        m.push(
            CheckmarkItem {
                label: "Avisar si un contenedor se cae".into(),
                checked: self.avisos.load(Ordering::Relaxed),
                activate: Box::new(|t: &mut Self| {
                    let on = !t.avisos.load(Ordering::Relaxed);
                    t.avisos.store(on, Ordering::Relaxed);
                    Settings::set_bool("avisos", on);
                }),
                ..Default::default()
            }
            .into(),
        );
        m.push(MenuItem::Separator);
        m.push(item("Mostrar ventana".into(), true, |_| launch_window()));
        m.push(item("Salir".into(), true, |_| std::process::exit(0)));
        m
    }
}

pub fn run() {
    // Instancia única: un socket abstracto que el kernel suelta al morir.
    let name = format!("zendock-bandeja-{}", util::uid());
    let Ok(addr) = SocketAddr::from_abstract_name(name.as_bytes()) else { return };
    let Ok(_lock) = UnixListener::bind_addr(&addr) else {
        return; // ya hay una bandeja: el autoarranque no debe duplicarla
    };

    let avisos = Arc::new(AtomicBool::new(Settings::get_bool("avisos", true)));
    let (snap_tx, snap_rx) = mpsc::channel::<Arc<Snapshot>>();
    let avisos_n = Arc::clone(&avisos);
    let tx = engine::start(
        false,
        Hooks {
            publish: Box::new(move |s| {
                let _ = snap_tx.send(s);
            }),
            notice: Some(Box::new(move |title, body| {
                if avisos_n.load(Ordering::Relaxed) {
                    notify::send(title, body);
                }
            })),
            error: Box::new(|e| notify::send("ZenDock", &e)),
        },
    );

    let tray = ZenTray {
        tx,
        snap: None,
        avisos,
        png_blue: icon::png(Accent::Blue, 32),
        png_grey: icon::png(Accent::Grey, 32),
    };
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("zendock: no se pudo crear el icono de la bandeja: {e}");
            std::process::exit(1);
        }
    };
    for snap in snap_rx {
        if handle.update(|t| t.snap = Some(snap)).is_none() {
            break;
        }
    }
}
