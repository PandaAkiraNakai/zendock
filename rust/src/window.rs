//! `zendock`: la ventana. Es un proceso aparte que solo vive mientras está
//! abierta; al cerrarla termina y libera la memoria. Una sola instancia: si ya
//! hay una, se le pide que pase al frente y esta termina.
//!
//! Todo se dibuja a mano con el pintor de egui para calcar la versión de Qt:
//! fondo oscuro, tarjetas, los mismos colores y medidas.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{
    Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Galley, Painter, Pos2,
    Rect, Response, Sense, Shape, Stroke, StrokeKind, TextFormat, Ui, UiBuilder, pos2, text::LayoutJob, vec2,
};

use crate::apps::{self, DesktopApp};
use crate::config::Settings;
use crate::engine::{self, Hooks, Msg, Snapshot, Verb};
use crate::gate::Power;
use crate::model::{Accent, Ctr, Group, size, status_of};
use crate::soft::{self, SoftApp};
use crate::{icon, util};

// ------------------------------------------------------------------ colores
const BG: Color32 = Color32::from_rgb(0x20, 0x23, 0x26);
const TEXT: Color32 = Color32::from_rgb(0xfc, 0xfc, 0xfc);
const POPUP: Color32 = Color32::from_rgb(0x29, 0x2c, 0x30);

fn white(alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, (alpha * 255.0).round() as u8)
}
fn card_fill() -> Color32 {
    white(0.055)
}
fn edge() -> Color32 {
    white(0.09)
}
fn dim() -> Color32 {
    white(0.55)
}
fn hover() -> Color32 {
    white(0.10)
}
fn disabled() -> Color32 {
    Color32::from_rgba_unmultiplied(0xfc, 0xfc, 0xfc, 100)
}
fn accent(a: Accent) -> Color32 {
    let [r, g, b] = a.rgb();
    Color32::from_rgb(r, g, b)
}

fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}
fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

// -------------------------------------------------------------- instancia única
fn instance_addr() -> Option<SocketAddr> {
    SocketAddr::from_abstract_name(format!("zendock-ventana-{}", util::uid()).as_bytes()).ok()
}

/// Trae al frente la ventana que ya está abierta.
fn focus_existing() {
    if let Some(addr) = instance_addr()
        && let Ok(mut s) = UnixStream::connect_addr(&addr)
    {
        let _ = s.write_all(b"focus\n");
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        // Hyprland con configuración en Lua; si no, la sintaxis clásica
        let lua = std::process::Command::new("hyprctl")
            .args(["dispatch", "hl.dsp.focus({ window = \"class:^(zendock)$\" })"])
            .output();
        let ok = lua
            .as_ref()
            .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).contains("rror"));
        if !ok {
            let _ = std::process::Command::new("hyprctl")
                .args(["dispatch", "focuswindow", "class:^(zendock)$"])
                .output();
        }
    }
}

// --------------------------------------------------------------------- fuentes
fn fc_match(pattern: &str) -> Option<String> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let f = String::from_utf8(out.stdout).ok()?;
    (!f.is_empty() && std::path::Path::new(&f).is_file()).then_some(f)
}

/// La fuente del sistema (la misma que usaba Qt), en normal, seminegrita y negrita.
fn system_fonts() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let Some(regular) = fc_match("sans-serif") else { return defs };
    let sibling = |weight: &str, pattern: &str| {
        let guess = regular.replace("-Regular.", &format!("-{weight}."));
        if guess != regular && std::path::Path::new(&guess).is_file() {
            Some(guess)
        } else {
            fc_match(pattern).filter(|f| *f != regular)
        }
    };
    let semi = sibling("SemiBold", "sans-serif:semibold").or_else(|| sibling("Medium", "sans-serif:medium"));
    let bolder = sibling("Bold", "sans-serif:bold");
    let mut load = |key: &str, path: &str| -> bool {
        match std::fs::read(path) {
            Ok(bytes) => {
                defs.font_data.insert(key.into(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            Err(_) => false,
        }
    };
    if !load("sys", &regular) {
        return defs;
    }
    let has_semi = semi.as_deref().is_some_and(|p| load("sys-semi", p));
    let has_bold = bolder.as_deref().is_some_and(|p| load("sys-bold", p));
    // respaldo para símbolos que no trae la fuente principal (→ de los puertos)
    let extra =
        fc_match("sans-serif:charset=2192").filter(|f| *f != regular).is_some_and(|p| load("sys-extra", &p));
    let mut fallback = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if extra {
        fallback.insert(0, "sys-extra".into());
    }
    let family = |first: &[&str]| -> Vec<String> {
        first.iter().map(|s| s.to_string()).chain(fallback.iter().cloned()).collect()
    };
    let semi_list = if has_semi { family(&["sys-semi", "sys"]) } else { family(&["sys"]) };
    let bold_list = if has_bold { family(&["sys-bold", "sys"]) } else { semi_list.clone() };
    defs.families.insert(FontFamily::Proportional, family(&["sys"]));
    defs.families.insert(FontFamily::Name("semibold".into()), semi_list);
    defs.families.insert(FontFamily::Name("bold".into()), bold_list);
    defs
}

fn ensure_families(defs: &mut FontDefinitions) {
    let base = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    for name in ["semibold", "bold"] {
        defs.families.entry(FontFamily::Name(name.into())).or_insert_with(|| base.clone());
    }
}

fn style(ctx: &egui::Context) {
    let mut defs = system_fonts();
    ensure_families(&mut defs);
    ctx.set_fonts(defs);
    ctx.global_style_mut(|s| {
        use egui::TextStyle::*;
        s.text_styles.insert(Body, regular(13.0));
        s.text_styles.insert(Button, regular(13.0));
        s.text_styles.insert(Small, regular(11.0));
        s.spacing.menu_margin = egui::Margin::same(4);
        s.spacing.button_padding = vec2(10.0, 4.0);
        s.spacing.scroll = egui::style::ScrollStyle {
            floating: false,
            bar_width: 8.0,
            handle_min_length: 32.0,
            bar_inner_margin: 0.0,
            bar_outer_margin: 0.0,
            ..egui::style::ScrollStyle::solid()
        };
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = POPUP;
        v.window_stroke = Stroke::new(1.0, edge());
        v.window_corner_radius = CornerRadius::same(7);
        v.menu_corner_radius = CornerRadius::same(7);
        v.override_text_color = Some(TEXT);
        v.extreme_bg_color = BG;
        let w = &mut v.widgets;
        w.inactive.bg_fill = white(0.16);
        w.inactive.weak_bg_fill = Color32::TRANSPARENT;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.hovered.bg_fill = dim();
        w.hovered.weak_bg_fill = Color32::from_rgba_unmultiplied(0x3d, 0xae, 0xe9, 90);
        w.hovered.bg_stroke = Stroke::NONE;
        w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
        w.active.bg_fill = dim();
        w.active.weak_bg_fill = Color32::from_rgba_unmultiplied(0x3d, 0xae, 0xe9, 130);
        w.active.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.bg_stroke = Stroke::new(1.0, edge());
    });
}

// ---------------------------------------------------------------------- texto
fn galley(ui: &Ui, s: &str, font: FontId, color: Color32, spacing: f32, max_w: Option<f32>) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(
        s.to_owned(),
        TextFormat { font_id: font, color, extra_letter_spacing: spacing, ..Default::default() },
    );
    if let Some(w) = max_w {
        job.wrap.max_width = w.max(8.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
    }
    ui.painter().layout_job(job)
}

/// Dibuja el texto con su borde izquierdo en `x` y centrado en `cy`.
fn put(ui: &Ui, g: &Arc<Galley>, x: f32, cy: f32) {
    ui.painter().galley(pos2(x, (cy - g.size().y / 2.0).round()), g.clone(), TEXT);
}

// ---------------------------------------------------------------------- iconos
#[derive(Clone, Copy)]
enum Ico {
    Play,
    Stop,
    Refresh,
    Folder,
    More,
    Globe,
}

fn arc(c: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    (0..=16)
        .map(|i| {
            let a = (from + (to - from) * i as f32 / 16.0).to_radians();
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

fn arrow_head(p: &Painter, c: Pos2, r: f32, at: f32, color: Color32) {
    let a = at.to_radians();
    let end = c + vec2(a.cos(), a.sin()) * r;
    let tangent = vec2(-a.sin(), a.cos());
    let normal = vec2(a.cos(), a.sin());
    p.add(Shape::convex_polygon(
        vec![end + tangent * 2.6, end + normal * 2.6 - tangent * 0.6, end - normal * 2.6 - tangent * 0.6],
        color,
        Stroke::NONE,
    ));
}

fn draw_icon(p: &Painter, ico: Ico, c: Pos2, color: Color32) {
    match ico {
        Ico::Play => {
            p.add(Shape::convex_polygon(
                vec![c + vec2(-4.5, -6.0), c + vec2(6.0, 0.0), c + vec2(-4.5, 6.0)],
                color,
                Stroke::NONE,
            ));
        }
        Ico::Stop => {
            p.rect_filled(Rect::from_center_size(c, vec2(12.0, 12.0)), CornerRadius::same(1), color);
        }
        Ico::Refresh => {
            let s = Stroke::new(1.6, color);
            p.add(Shape::line(arc(c, 5.6, 195.0, 325.0), s));
            p.add(Shape::line(arc(c, 5.6, 15.0, 145.0), s));
            arrow_head(p, c, 5.6, 330.0, color);
            arrow_head(p, c, 5.6, 150.0, color);
        }
        Ico::Folder => {
            let body = Rect::from_min_max(c + vec2(-6.5, -4.0), c + vec2(6.5, 5.5));
            p.rect_stroke(body, CornerRadius::same(1), Stroke::new(1.5, color), StrokeKind::Inside);
            p.rect_filled(
                Rect::from_min_max(c + vec2(-6.5, -6.0), c + vec2(-1.0, -3.0)),
                CornerRadius::same(1),
                color,
            );
            p.rect_filled(
                Rect::from_min_max(c + vec2(-6.5, -4.0), c + vec2(6.5, -2.0)),
                CornerRadius::ZERO,
                color,
            );
        }
        Ico::More => {
            for dy in [-4.5, 0.0, 4.5] {
                p.circle_filled(c + vec2(0.0, dy), 1.4, color);
            }
        }
        Ico::Globe => {
            let alpha = color.a();
            let tint = |c: Color32| Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha);
            p.circle_filled(c, 6.5, tint(Color32::from_rgb(0x1b, 0x8e, 0xe2)));
            let land = tint(Color32::from_rgb(0x6e, 0xd9, 0xaa));
            p.circle_filled(c + vec2(-2.2, -2.0), 2.4, land);
            p.circle_filled(c + vec2(2.6, 1.6), 2.1, land);
            p.circle_filled(c + vec2(-1.4, 3.6), 1.4, land);
        }
    }
}

// --------------------------------------------------------------------- botones
const TOOL_W: f32 = 27.0;
const TOOL_H: f32 = 26.0;
const BTN_H: f32 = 23.0;

enum BtnIcon {
    None,
    Ico(Ico),
    Tex(egui::TextureId),
}

/// Botón de herramienta: solo icono, fondo al pasar por encima.
fn tool(
    ui: &mut Ui,
    id: egui::Id,
    right: f32,
    cy: f32,
    ico: Ico,
    enabled: bool,
    tip: &str,
) -> (Response, f32) {
    let rect = Rect::from_min_size(pos2(right - TOOL_W, cy - TOOL_H / 2.0), vec2(TOOL_W, TOOL_H));
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    if enabled && (resp.hovered() || resp.is_pointer_button_down_on()) {
        ui.painter().rect_filled(rect, CornerRadius::same(7), hover());
        ui.painter().rect_stroke(rect, CornerRadius::same(7), Stroke::new(1.0, edge()), StrokeKind::Inside);
    }
    draw_icon(ui.painter(), ico, rect.center(), if enabled { TEXT } else { disabled() });
    let mut resp = resp.on_hover_text(tip);
    if enabled {
        resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    }
    (resp, rect.left())
}

/// Botón con texto (y quizá icono), con su marco de tarjeta.
fn action(
    ui: &mut Ui,
    id: egui::Id,
    right: f32,
    cy: f32,
    label: &str,
    ico: BtnIcon,
    enabled: bool,
) -> (Response, f32) {
    let color = if enabled { TEXT } else { dim() };
    let g = galley(ui, label, regular(11.0), color, 0.0, None);
    let icon_w = if matches!(ico, BtnIcon::None) { 0.0 } else { 18.0 };
    let w = 1.0 + 9.0 + icon_w + g.size().x + 9.0 + 1.0;
    let rect =
        Rect::from_min_size(pos2((right - w).round(), (cy - BTN_H / 2.0).round()), vec2(w.round(), BTN_H));
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    let fill = if enabled && resp.hovered() { hover() } else { card_fill() };
    ui.painter().rect_filled(rect, CornerRadius::same(7), fill);
    ui.painter().rect_stroke(rect, CornerRadius::same(7), Stroke::new(1.0, edge()), StrokeKind::Inside);
    let mut x = rect.left() + 10.0;
    let icy = rect.center().y;
    match ico {
        BtnIcon::None => {}
        BtnIcon::Ico(i) => {
            let tint = if enabled { TEXT } else { disabled() };
            draw_icon(ui.painter(), i, pos2(x + 7.0, icy), tint);
            x += icon_w;
        }
        BtnIcon::Tex(t) => {
            let r = Rect::from_center_size(pos2(x + 7.0, icy), vec2(14.0, 14.0));
            let tint = if enabled { Color32::WHITE } else { white(0.5) };
            ui.painter().image(t, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), tint);
            x += icon_w;
        }
    }
    put(ui, &g, x, icy);
    let resp = if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    (resp, rect.left())
}

fn menu_item(ui: &mut Ui, label: &str, enabled: bool) -> bool {
    ui.add_enabled(enabled, egui::Button::new(label).frame_when_inactive(false)).clicked()
}

// ------------------------------------------------------------------------ app
struct Shared {
    snap: Mutex<Option<Arc<Snapshot>>>,
    note: Mutex<Option<String>>,
    focus: AtomicBool,
}

struct App {
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    only_running: bool,
    icons: HashMap<String, Option<egui::TextureHandle>>,
    size: Option<egui::Vec2>,
    /// ZENDOCK_CAPTURA=archivo.png: guarda un fotograma a los 4 s y cierra
    /// (para documentar y para probar sin mirar la pantalla).
    capture: Option<(String, std::time::Instant, bool)>,
}

fn save_capture(path: &str, img: &egui::ColorImage) {
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("zendock: no se pudo crear {path}: {e}");
            std::process::exit(1);
        }
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let ok = enc.write_header().and_then(|mut w| w.write_image_data(img.as_raw())).is_ok();
    std::process::exit(if ok { 0 } else { 1 });
}

impl App {
    fn note(&self, text: impl Into<String>) {
        *self.shared.note.lock().unwrap() = Some(text.into());
    }

    fn app_texture(&mut self, ctx: &egui::Context, app: &DesktopApp) -> BtnIcon {
        let entry = self.icons.entry(app.icon.clone()).or_insert_with(|| {
            let path = apps::icon_png_path(&app.icon)?;
            let (w, h, rgba) = icon::load_png(&path)?;
            let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
            Some(ctx.load_texture(&app.icon, img, egui::TextureOptions::LINEAR))
        });
        match entry {
            Some(t) => BtnIcon::Tex(t.id()),
            None => BtnIcon::None,
        }
    }

    fn launch(&self, app: &DesktopApp) {
        if !apps::launch_app(app) {
            self.note(format!("No se pudo abrir {}.", app.name));
        }
    }

    // ---------------------------------------------------------------- resumen
    fn top_row(&self, ui: &mut Ui, r: Rect, snap: Option<&Snapshot>) {
        let on = snap.is_some_and(|s| s.power == Power::On && s.connected);
        let stat = Rect::from_min_size(r.min, vec2(96.0, r.height()));
        let mw = ((r.width() - 96.0 - 16.0) / 2.0).floor();
        let cpu_r = Rect::from_min_size(pos2(stat.right() + 8.0, r.top()), vec2(mw, r.height()));
        let ram_r = Rect::from_min_max(pos2(cpu_r.right() + 8.0, r.top()), r.max);
        for rect in [stat, cpu_r, ram_r] {
            ui.painter().rect_filled(rect, CornerRadius::same(9), card_fill());
            ui.painter().rect_stroke(
                rect,
                CornerRadius::same(9),
                Stroke::new(1.0, edge()),
                StrokeKind::Inside,
            );
        }
        // En marcha
        let key = galley(ui, "EN MARCHA", regular(10.0), dim(), 0.6, None);
        put(ui, &key, stat.left() + 11.0, stat.top() + 16.0);
        let run = match snap {
            Some(s) if on => format!("{} de {}", s.running, s.total),
            _ => "--".into(),
        };
        let val = galley(ui, &run, semibold(15.0), TEXT, 0.0, None);
        put(ui, &val, stat.left() + 11.0, stat.top() + 35.0);

        // CPU y RAM
        let (mut cpu, mut mem, ncpu, total) = (0.0, 0u64, 1usize, 0u64);
        let (ncpu, total) = match snap {
            Some(s) => {
                for c in s.ctrs().filter(|c| c.running()) {
                    cpu += c.cpu.unwrap_or(0.0);
                    mem += c.mem.unwrap_or(0);
                }
                (s.ncpu.max(1), s.mem_total)
            }
            None => (ncpu, total),
        };
        let cpu = cpu / ncpu as f64;
        let frac_mem = if total > 0 { mem as f64 / total as f64 } else { 0.0 };
        let meters = [
            (cpu_r, "CPU", format!("de {ncpu} hilos"), format!("{cpu:.1} %"), cpu / 100.0, cpu >= 85.0),
            (ram_r, "RAM", format!("de {}", size(Some(total))), size(Some(mem)), frac_mem, frac_mem >= 0.85),
        ];
        for (rect, title, sub, value, frac, hot) in meters {
            let cy = rect.top() + 22.0;
            let k = galley(ui, title, regular(10.0), dim(), 0.6, None);
            put(ui, &k, rect.left() + 11.0, cy);
            let s = galley(ui, &sub, regular(11.0), dim(), 0.0, None);
            put(ui, &s, rect.left() + 11.0 + k.size().x + 6.0, cy);
            let v = galley(ui, if on { &value } else { "--" }, semibold(13.0), TEXT, 0.0, None);
            put(ui, &v, rect.right() - 11.0 - v.size().x, cy);
            let bar = Rect::from_min_max(
                pos2(rect.left() + 11.0, rect.top() + 41.0),
                pos2(rect.right() - 11.0, rect.top() + 46.0),
            );
            ui.painter().rect_filled(
                bar,
                CornerRadius::same(3),
                Color32::from_rgba_unmultiplied(0xfc, 0xfc, 0xfc, 30),
            );
            let frac = if on { frac.clamp(0.0, 1.0) as f32 } else { 0.0 };
            if frac > 0.004 {
                let fill =
                    Rect::from_min_size(bar.min, vec2((bar.width() * frac).max(bar.height()), bar.height()));
                let color = accent(if hot { Accent::Amber } else { Accent::Blue });
                ui.painter().rect_filled(fill, CornerRadius::same(3), color);
            }
        }
    }

    // ------------------------------------------------------- barra de sección
    fn section_row(&mut self, ui: &mut Ui, r: Rect) {
        let g = galley(ui, "CONTENEDORES", bold(10.0), dim(), 1.1, None);
        put(ui, &g, r.left() + 3.0, r.center().y + 1.0);
        let on = self.only_running;
        let label = galley(
            ui,
            "Solo en marcha",
            if on { bold(11.0) } else { regular(11.0) },
            if on { Color32::WHITE } else { TEXT },
            0.0,
            None,
        );
        let w = (label.size().x + 22.0).round();
        let chip = Rect::from_min_size(pos2(r.right() - w, r.top()), vec2(w, BTN_H));
        let resp = ui
            .interact(chip, egui::Id::unique("chip"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        let blue = accent(Accent::Blue);
        let (fill, stroke) = if on {
            (blue, blue)
        } else if resp.hovered() {
            (hover(), edge())
        } else {
            (card_fill(), edge())
        };
        ui.painter().rect_filled(chip, CornerRadius::same(8), fill);
        ui.painter().rect_stroke(chip, CornerRadius::same(8), Stroke::new(1.0, stroke), StrokeKind::Inside);
        put(ui, &label, chip.left() + 11.0, chip.center().y);
        if resp.clicked() {
            self.only_running = !on;
            Settings::set_bool("solo_en_marcha", self.only_running);
        }
    }

    // ------------------------------------------------------------- cabecera
    fn header(&mut self, ui: &mut Ui, g: &Group, busy: Option<&str>) {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
        let id = egui::Id::unique(("cab", &g.project));
        let cy = r.top() + 22.0;
        let title = galley(ui, &g.title.to_uppercase(), bold(10.0), dim(), 1.1, None);
        put(ui, &title, r.left() + 5.0, cy);
        let running = g.running();
        let count =
            busy.map(str::to_owned).unwrap_or_else(|| format!("{running}/{} en marcha", g.ctrs.len()));
        let cg = galley(ui, &count, regular(11.0), dim(), 0.0, None);
        put(ui, &cg, r.left() + 5.0 + title.size().x + 12.0, cy);

        let is_project = !g.project.is_empty();
        let mut right = r.right() - 6.0;
        if is_project {
            if std::path::Path::new(&g.workdir).is_dir() {
                let (resp, l) =
                    tool(ui, id.with("dir"), right, cy, Ico::Folder, true, "Abrir la carpeta del proyecto");
                if resp.clicked() {
                    util::open_url(&g.workdir);
                }
                right = l - 4.0;
            }
            let any_off = g.ctrs.iter().any(|c| !c.alive());
            let any_on = g.ctrs.iter().any(|c| c.alive());
            for (verb, ico, enabled, tip) in [
                (Verb::Restart, Ico::Refresh, running > 0, "Reiniciar todo el proyecto"),
                (Verb::Stop, Ico::Stop, any_on, "Apagar todo el proyecto"),
                (Verb::Start, Ico::Play, any_off, "Encender todo el proyecto"),
            ] {
                let (resp, l) = tool(ui, id.with(verb.api()), right, cy, ico, enabled && busy.is_none(), tip);
                if resp.clicked() {
                    let _ = self.tx.send(Msg::Project { project: g.project.clone(), verb });
                }
                right = l - 4.0;
            }
        }
        let ctx = ui.ctx().clone();
        for (i, app) in g.apps.iter().enumerate().rev() {
            let ico = self.app_texture(&ctx, app);
            let (resp, l) = action(ui, id.with(("app", i)), right, cy, &app.name, ico, true);
            if resp.on_hover_text(format!("Abrir {}", app.name)).clicked() {
                self.launch(app);
            }
            right = l - 3.0;
        }
    }

    // --------------------------------------------------------------- tarjeta
    fn card(&mut self, ui: &mut Ui, c: &Ctr, busy: Option<&str>) {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 62.0), Sense::hover());
        let id = egui::Id::unique(("ctr", &c.id));
        let card_hover = ui.interact(r, id.with("tip"), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::same(9), card_fill());
        ui.painter().rect_stroke(r, CornerRadius::same(9), Stroke::new(1.0, edge()), StrokeKind::Inside);

        let (color, mut text) = status_of(c);
        let top = r.top() + 17.0;
        let bottom = r.top() + 41.0;
        let left = r.left() + 11.0;
        let dot = if busy.is_some() { Accent::Amber } else { color };
        ui.painter().circle_filled(pos2(left + 5.0, top), 4.0, accent(dot));
        let name_color = if c.alive() { TEXT } else { white(0.42) };
        let usage = if c.running() {
            let cpu = c.cpu.map(|v| format!("{v:.1} %")).unwrap_or("--".into());
            format!("{cpu}  ·  {}", size(c.mem))
        } else {
            String::new()
        };
        let ug = galley(ui, &usage, regular(11.0), dim(), 0.0, None);
        let usage_x = r.right() - 11.0 - ug.size().x;
        put(ui, &ug, usage_x, top);
        let ng = galley(ui, &c.name, semibold(13.0), name_color, 0.0, Some(usage_x - (left + 17.0) - 8.0));
        put(ui, &ng, left + 17.0, top);

        if c.running() {
            if c.net_mode == "host" {
                text += " · red del host";
            } else if !c.ports.is_empty() {
                let ports: Vec<String> = c.ports.iter().map(|p| format!(":{}", p.host)).collect();
                text += &format!(" · {}", ports.join(" "));
            }
        }

        // botones, de derecha a izquierda
        let mut hovered = false;
        let mut right = r.right() - 7.0;
        let (more, l) = tool(ui, id.with("mas"), right, bottom, Ico::More, true, "Más");
        hovered |= more.hovered();
        right = l - 3.0;
        let (rst, l) =
            tool(ui, id.with("rst"), right, bottom, Ico::Refresh, busy.is_none() && c.running(), "Reiniciar");
        hovered |= rst.hovered();
        if rst.clicked() {
            let _ = self.tx.send(Msg::Container { id: c.id.clone(), verb: Verb::Restart });
        }
        right = l - 3.0;
        let alive = c.alive();
        let (pwr, l) = tool(
            ui,
            id.with("pwr"),
            right,
            bottom,
            if alive { Ico::Stop } else { Ico::Play },
            busy.is_none(),
            if alive { "Apagar" } else { "Encender" },
        );
        hovered |= pwr.hovered();
        if pwr.clicked() {
            let verb = if alive { Verb::Stop } else { Verb::Start };
            let _ = self.tx.send(Msg::Container { id: c.id.clone(), verb });
        }
        right = l - 3.0;
        if let Some(app) = c.apps.first() {
            let ctx = ui.ctx().clone();
            let ico = self.app_texture(&ctx, app);
            let (resp, l) = action(ui, id.with("app"), right, bottom, &app.name, ico, true);
            hovered |= resp.hovered();
            if resp.on_hover_text(format!("Abrir {}", app.name)).clicked() {
                self.launch(app);
            }
            right = l - 3.0;
        }
        if !c.webs.is_empty() {
            let enabled = c.running();
            let (resp, l) =
                action(ui, id.with("web"), right, bottom, "Web", BtnIcon::Ico(Ico::Globe), enabled);
            hovered |= resp.hovered();
            let tip = if enabled {
                c.webs.iter().map(|(_, u)| u.as_str()).collect::<Vec<_>>().join("\n")
            } else {
                "Enciéndelo para abrir su web".into()
            };
            let resp = resp.on_hover_text(tip);
            if c.webs.len() == 1 {
                if resp.clicked() {
                    util::open_url(&c.webs[0].1);
                }
            } else {
                egui::Popup::menu(&resp).show(|ui| {
                    for (name, url) in &c.webs {
                        if menu_item(ui, &format!("{name}  ·  {url}"), true) {
                            util::open_url(url);
                        }
                    }
                });
            }
            right = l - 3.0;
        }
        let sub = busy.map(str::to_owned).unwrap_or(text);
        let sg = galley(ui, &sub, regular(11.0), dim(), 0.0, Some(right - 3.0 - (left + 17.0)));
        put(ui, &sg, left + 17.0, bottom);

        self.more_menu(&more, c);

        if !hovered {
            let mut tip =
                vec![format!("Imagen: {}", c.image), format!("ID: {}", &c.id[..c.id.len().min(12)])];
            if !c.project.is_empty() {
                tip.push(format!("Proyecto: {} · servicio {}", c.project, c.service));
            }
            if !c.ports.is_empty() {
                let p: Vec<String> = c
                    .ports
                    .iter()
                    .map(|p| {
                        format!("{}:{} → {}", if p.ip.is_empty() { "0.0.0.0" } else { &p.ip }, p.host, p.ctr)
                    })
                    .collect();
                tip.push(format!("Puertos: {}", p.join(", ")));
            }
            if !c.ips.is_empty() {
                let ips: Vec<String> = c.ips.iter().map(|(net, ip)| format!("{ip} ({net})")).collect();
                tip.push(format!("IPs: {}", ips.join(", ")));
            }
            if !c.restart_policy.is_empty() && c.restart_policy != "no" {
                tip.push(format!("Reinicio: {} · {} reinicios", c.restart_policy, c.restarts));
            }
            card_hover.on_hover_text(tip.join("\n"));
        }
    }

    fn more_menu(&self, resp: &Response, c: &Ctr) {
        egui::Popup::menu(resp).show(|ui| {
            ui.set_min_width(200.0);
            if menu_item(ui, "Ver logs", true) {
                let title = format!("logs {}", c.name);
                if !util::in_terminal(&title, &["docker", "logs", "-f", "--tail", "300", &c.name]) {
                    self.note("No hay un terminal para enseñar los logs (instala kitty o konsole).");
                }
            }
            if menu_item(ui, "Abrir terminal dentro", c.running()) {
                let cmd = [
                    "docker",
                    "exec",
                    "-it",
                    &c.name,
                    "sh",
                    "-c",
                    "command -v bash >/dev/null && exec bash || exec sh",
                ];
                if !util::in_terminal(&c.name, &cmd) {
                    self.note("No hay un terminal para abrir la shell (instala kitty o konsole).");
                }
            }
            if matches!(c.state.as_str(), "running" | "paused") {
                let paused = c.state == "paused";
                if menu_item(ui, if paused { "Reanudar" } else { "Pausar" }, true) {
                    let verb = if paused { Verb::Unpause } else { Verb::Pause };
                    let _ = self.tx.send(Msg::Container { id: c.id.clone(), verb });
                }
            }
            ui.separator();
            for (net, ip) in &c.ips {
                if menu_item(ui, &format!("Copiar IP  {ip}  ({net})"), true) {
                    util::copy_text(ip);
                }
            }
            if menu_item(ui, "Copiar nombre", true) {
                util::copy_text(&c.name);
            }
            if !c.workdir.is_empty()
                && std::path::Path::new(&c.workdir).is_dir()
                && menu_item(ui, "Abrir carpeta del proyecto", true)
            {
                util::open_url(&c.workdir);
            }
        });
    }

    // ------------------------------------------------------- Docker apagado
    fn docker_off(&mut self, ui: &mut Ui, r: Rect, snap: Option<&Snapshot>) {
        let power = snap.map(|s| s.power).unwrap_or(Power::Unknown);
        let waking = snap.is_some_and(|s| s.waking);
        let (title, sub, button) = match power {
            Power::Off if waking => ("Encendiendo Docker…", "Esperando a que dockerd arranque.", None),
            Power::Off => (
                "Docker está apagado",
                "Con docker.socket activo, se enciende en cuanto algo lo usa. También puedes encenderlo desde aquí.",
                Some("Encender Docker"),
            ),
            Power::Starting => ("Encendiendo Docker…", "Esperando a que dockerd arranque.", None),
            Power::Stopping => ("Docker se está apagando…", "Volverá a aparecer cuando lo enciendas.", None),
            Power::Unknown => ("Comprobando Docker…", "", None),
            Power::On => match snap.and_then(|s| s.error.as_deref()) {
                Some(_) => ("No se puede hablar con Docker", "Mira el detalle abajo.", None),
                None => ("Conectando con Docker…", "", None),
            },
        };
        let h = if button.is_some() { 118.0 } else { 78.0 };
        let card = Rect::from_min_size(r.min, vec2(r.width() - 4.0, h));
        ui.painter().rect_filled(card, CornerRadius::same(9), card_fill());
        ui.painter().rect_stroke(card, CornerRadius::same(9), Stroke::new(1.0, edge()), StrokeKind::Inside);
        let dot = match power {
            Power::Starting | Power::Stopping => Accent::Amber,
            _ if waking => Accent::Amber,
            _ => Accent::Grey,
        };
        ui.painter().circle_filled(pos2(card.left() + 16.0, card.top() + 24.0), 4.0, accent(dot));
        let t = galley(ui, title, semibold(13.0), TEXT, 0.0, None);
        put(ui, &t, card.left() + 28.0, card.top() + 24.0);
        let s = galley(ui, sub, regular(11.0), dim(), 0.0, Some(card.width() - 40.0));
        put(ui, &s, card.left() + 28.0, card.top() + 48.0);
        if let Some(label) = button {
            let id = egui::Id::unique("encender");
            let (resp, _) =
                action(ui, id, card.right() - 12.0, card.top() + 88.0, label, BtnIcon::Ico(Ico::Play), true);
            if resp.clicked() {
                let _ = self.tx.send(Msg::WakeDocker);
            }
        }
    }

    // ------------------------------------------------------------------ lista
    fn list(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let hide_stopped = self.only_running;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 6.0);
            let width = ui.available_width() - 4.0;
            ui.set_width(width);
            let mut any = false;
            for g in &snap.groups {
                let busy_p = snap.busy.get(&g.key()).copied();
                let shown: Vec<&Ctr> = g
                    .ctrs
                    .iter()
                    .filter(|c| !hide_stopped || c.alive() || snap.busy.contains_key(&c.id))
                    .collect();
                if shown.is_empty() && busy_p.is_none() {
                    continue;
                }
                any = true;
                self.header(ui, g, busy_p);
                for c in shown {
                    self.card(ui, c, snap.busy.get(&c.id).copied());
                }
            }
            if !any {
                let msg =
                    if snap.total == 0 { "No hay contenedores." } else { "Ningún contenedor en marcha." };
                ui.add_space(8.0);
                let g = galley(ui, msg, regular(11.0), dim(), 0.0, None);
                let (r, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
                put(ui, &g, r.left() + 5.0, r.center().y);
            }
        });
    }
}

impl SoftApp for App {
    fn ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            self.size = Some(r.size());
        }
        if self.shared.focus.swap(false, Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        let snap = self.shared.snap.lock().unwrap().clone();
        // el fondo lo pone el backend al limpiar (clear_color)
        let full = ui.max_rect();
        let inner = Rect::from_min_max(full.min + vec2(16.0, 14.0), full.max - vec2(16.0, 14.0));

        let top = Rect::from_min_size(inner.min, vec2(inner.width(), 55.0));
        self.top_row(ui, top, snap.as_deref());
        let sec = Rect::from_min_size(pos2(inner.left(), top.bottom() + 10.0), vec2(inner.width(), BTN_H));
        self.section_row(ui, sec);

        // nota de abajo: errores, enlaces.json mal escrito…
        let note = {
            let transient = self.shared.note.lock().unwrap().clone();
            match snap.as_deref() {
                None => Some("Conectando con Docker…".to_owned()),
                Some(s) if s.power == Power::On && !s.connected && s.error.is_some() => {
                    Some(format!("No se puede hablar con Docker: {}", s.error.as_deref().unwrap_or("")))
                }
                Some(s) if !s.config_error.is_empty() => Some(s.config_error.clone()),
                _ => transient,
            }
        };
        let note_g = note.map(|n| {
            let mut job = LayoutJob::single_section(
                n,
                TextFormat { font_id: regular(10.0), color: dim(), ..Default::default() },
            );
            job.wrap.max_width = inner.width();
            ui.painter().layout_job(job)
        });
        let note_h = note_g.as_ref().map(|g| g.size().y + 10.0).unwrap_or(0.0);
        let list_r = Rect::from_min_max(
            pos2(inner.left(), sec.bottom() + 10.0),
            pos2(full.right() - 16.0, inner.bottom() - note_h),
        );
        if let Some(g) = &note_g {
            ui.painter().galley(pos2(inner.left(), inner.bottom() - g.size().y), g.clone(), TEXT);
        }

        let mut child = ui.new_child(UiBuilder::new().max_rect(list_r));
        match snap.as_deref() {
            Some(s) if s.power == Power::On && s.connected => self.list(&mut child, s),
            other => self.docker_off(&mut child, list_r, other),
        }
        if let Some((path, t0, asked)) = &mut self.capture {
            if !*asked && t0.elapsed() > Duration::from_secs(4) {
                *asked = true;
                let path = path.clone();
                ctx.request_screenshot(move |img| save_capture(&path, &img));
            }
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        // los stats ya repintan cada 2 s; esto es para los "hace X min" sin nada en marcha
        ctx.request_repaint_after(Duration::from_secs(10));
    }

    fn clear_color(&self) -> Color32 {
        BG
    }

    fn on_exit(&mut self) {
        if let Some(s) = self.size {
            Settings::set_size(s.x, s.y);
        }
    }
}

pub fn run() {
    // Instancia única, antes de cargar nada pesado: así enfocar es inmediato.
    let Some(addr) = instance_addr() else { return };
    let listener = match UnixListener::bind_addr(&addr) {
        Ok(l) => l,
        Err(_) => {
            focus_existing();
            return;
        }
    };

    let (w, h) = Settings::get_size().unwrap_or((500.0, 760.0));
    let viewport = egui::ViewportBuilder::default()
        .with_title("ZenDock")
        .with_app_id("zendock")
        .with_inner_size([w, h])
        .with_min_inner_size([440.0, 300.0])
        .with_icon(egui::IconData { rgba: icon::rgba(Accent::Blue, 64), width: 64, height: 64 });
    let capture = std::env::var("ZENDOCK_CAPTURA").ok().filter(|p| !p.is_empty());
    let shared =
        Arc::new(Shared { snap: Mutex::new(None), note: Mutex::new(None), focus: AtomicBool::new(false) });
    let result = soft::run(viewport, move |ctx, waker| {
        style(ctx);
        {
            let (shared, ctx) = (Arc::clone(&shared), ctx.clone());
            std::thread::Builder::new()
                .name("instancia".into())
                .stack_size(64 * 1024)
                .spawn(move || {
                    for mut conn in listener.incoming().flatten() {
                        let mut buf = [0u8; 16];
                        let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
                        let _ = conn.read(&mut buf);
                        shared.focus.store(true, Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                })
                .ok();
        }
        let (s1, w1, s2, w2) = (Arc::clone(&shared), waker.clone(), Arc::clone(&shared), waker);
        let tx = engine::start(
            true,
            Hooks {
                publish: Box::new(move |snap| {
                    *s1.snap.lock().unwrap() = Some(snap);
                    w1.wake();
                }),
                notice: None,
                error: Box::new(move |e| {
                    *s2.note.lock().unwrap() = Some(e);
                    w2.wake();
                }),
            },
        );
        Box::new(App {
            shared,
            tx,
            only_running: Settings::get_bool("solo_en_marcha", false),
            icons: HashMap::new(),
            size: None,
            capture: capture.map(|p| (p, std::time::Instant::now(), false)),
        })
    });
    if let Err(e) = result {
        eprintln!("zendock: no se pudo abrir la ventana: {e}");
        std::process::exit(1);
    }
    // los hilos del motor no deben retrasar la salida
    std::process::exit(0);
}
