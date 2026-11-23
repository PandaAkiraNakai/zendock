//! Backend de software para egui: winit + softbuffer (wl_shm en Wayland) y un
//! rasterizador propio de las mallas de egui en la CPU.
//!
//! Así la ventana no carga el driver OpenGL (en Mesa, libLLVM y libgallium son
//! más de 50 MB de RSS por proceso). Para una ventana que solo se repinta con
//! eventos o cada dos segundos, pintar en la CPU sobra.
//!
//! El rasterizador trabaja en coma fija (1/256 de píxel) con funciones de
//! arista enteras y la regla top-left: los triángulos que comparten arista no
//! se solapan ni dejan huecos, así que los rellenos translúcidos no muestran
//! costuras. Los colores van premultiplicados y se mezclan en espacio gamma,
//! como espera egui.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use egui::epaint::{ClippedPrimitive, Primitive, TextureId};
use egui::{Color32, ViewportId};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

pub trait SoftApp {
    fn ui(&mut self, ui: &mut egui::Ui);
    fn on_exit(&mut self) {}
    fn clear_color(&self) -> Color32 {
        Color32::BLACK
    }
}

// ----------------------------------------------------------------- texturas
struct Tex {
    w: usize,
    h: usize,
    px: Vec<Color32>,
    linear: bool,
}

impl Tex {
    #[inline]
    fn texel(&self, x: i32, y: i32) -> Color32 {
        let x = x.clamp(0, self.w as i32 - 1) as usize;
        let y = y.clamp(0, self.h as i32 - 1) as usize;
        self.px[y * self.w + x]
    }

    #[inline]
    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        if !self.linear {
            let c = self.texel((u * self.w as f32).floor() as i32, (v * self.h as f32).floor() as i32);
            return [c.r() as f32, c.g() as f32, c.b() as f32, c.a() as f32];
        }
        let fx = u * self.w as f32 - 0.5;
        let fy = v * self.h as f32 - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);
        let c00 = self.texel(x0, y0);
        let c10 = self.texel(x0 + 1, y0);
        let c01 = self.texel(x0, y0 + 1);
        let c11 = self.texel(x0 + 1, y0 + 1);
        let mut out = [0.0; 4];
        for (i, o) in out.iter_mut().enumerate() {
            let a = c00.to_array()[i] as f32 * (1.0 - tx) + c10.to_array()[i] as f32 * tx;
            let b = c01.to_array()[i] as f32 * (1.0 - tx) + c11.to_array()[i] as f32 * tx;
            *o = a * (1.0 - ty) + b * ty;
        }
        out
    }
}

#[derive(Default)]
pub struct Raster {
    textures: HashMap<TextureId, Tex>,
}

const SUB: i64 = 256; // subpíxeles por píxel

#[inline]
fn edge(ax: i64, ay: i64, bx: i64, by: i64, px: i64, py: i64) -> i64 {
    (bx - ax) * (py - ay) - (by - ay) * (px - ax)
}

/// Aristas superiores o izquierdas (con el interior a su derecha, y hacia abajo).
#[inline]
fn top_left(ax: i64, ay: i64, bx: i64, by: i64) -> bool {
    (ay == by && bx > ax) || by < ay
}

impl Raster {
    pub fn set_textures(&mut self, delta: &egui::TexturesDelta) {
        for (id, d) in delta.set.iter().flat_map(|(id, ds)| ds.iter().map(move |d| (id, d))) {
            let egui::ImageData::Color(img) = &d.image;
            let linear = d.options.magnification == egui::TextureFilter::Linear;
            match d.pos {
                None => {
                    self.textures
                        .insert(*id, Tex { w: img.size[0], h: img.size[1], px: img.pixels.clone(), linear });
                }
                Some([x0, y0]) => {
                    if let Some(t) = self.textures.get_mut(id) {
                        for y in 0..img.size[1] {
                            for x in 0..img.size[0] {
                                let (tx, ty) = (x0 + x, y0 + y);
                                if tx < t.w && ty < t.h {
                                    t.px[ty * t.w + tx] = img.pixels[y * img.size[0] + x];
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn free_textures(&mut self, delta: &egui::TexturesDelta) {
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    /// Pinta en un búfer 0x00RRGGBB de `w`×`h`.
    pub fn render(
        &self,
        buf: &mut [u32],
        w: usize,
        h: usize,
        prims: &[ClippedPrimitive],
        ppp: f32,
        clear: Color32,
    ) {
        let bg = (clear.r() as u32) << 16 | (clear.g() as u32) << 8 | clear.b() as u32;
        buf[..w * h].fill(bg);
        for prim in prims {
            let Primitive::Mesh(mesh) = &prim.primitive else { continue };
            let Some(tex) = self.textures.get(&mesh.texture_id) else { continue };
            let clip = prim.clip_rect;
            let cx0 = ((clip.min.x * ppp).round().max(0.0) as i64).min(w as i64);
            let cy0 = ((clip.min.y * ppp).round().max(0.0) as i64).min(h as i64);
            let cx1 = ((clip.max.x * ppp).round().max(0.0) as i64).min(w as i64);
            let cy1 = ((clip.max.y * ppp).round().max(0.0) as i64).min(h as i64);
            if cx0 >= cx1 || cy0 >= cy1 {
                continue;
            }
            for tri in mesh.indices.as_chunks::<3>().0 {
                let v = [
                    &mesh.vertices[tri[0] as usize],
                    &mesh.vertices[tri[1] as usize],
                    &mesh.vertices[tri[2] as usize],
                ];
                self.triangle(buf, w, v, ppp, tex, [cx0, cy0, cx1, cy1]);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn triangle(
        &self,
        buf: &mut [u32],
        w: usize,
        v: [&egui::epaint::Vertex; 3],
        ppp: f32,
        tex: &Tex,
        clip: [i64; 4],
    ) {
        let fix = |f: f32| (f * ppp * SUB as f32).round() as i64;
        let (mut ax, mut ay) = (fix(v[0].pos.x), fix(v[0].pos.y));
        let (mut bx, mut by) = (fix(v[1].pos.x), fix(v[1].pos.y));
        let (cx, cy) = (fix(v[2].pos.x), fix(v[2].pos.y));
        let mut va = v[0];
        let mut vb = v[1];
        let vc = v[2];
        let mut area = edge(ax, ay, bx, by, cx, cy);
        if area == 0 {
            return;
        }
        if area < 0 {
            std::mem::swap(&mut ax, &mut bx);
            std::mem::swap(&mut ay, &mut by);
            std::mem::swap(&mut va, &mut vb);
            area = -area;
        }
        // caja del triángulo, en píxeles, recortada
        let minx = (ax.min(bx).min(cx) / SUB).max(clip[0]);
        let maxx = ((ax.max(bx).max(cx) + SUB - 1) / SUB).min(clip[2]);
        let miny = (ay.min(by).min(cy) / SUB).max(clip[1]);
        let maxy = ((ay.max(by).max(cy) + SUB - 1) / SUB).min(clip[3]);
        if minx >= maxx || miny >= maxy {
            return;
        }
        // arista i: la opuesta al vértice i
        let bias0 = !top_left(bx, by, cx, cy) as i64;
        let bias1 = !top_left(cx, cy, ax, ay) as i64;
        let bias2 = !top_left(ax, ay, bx, by) as i64;
        let (px0, py0) = (minx * SUB + SUB / 2, miny * SUB + SUB / 2);
        let mut row0 = edge(bx, by, cx, cy, px0, py0);
        let mut row1 = edge(cx, cy, ax, ay, px0, py0);
        let mut row2 = edge(ax, ay, bx, by, px0, py0);
        let (dx0, dy0) = (-(cy - by) * SUB, (cx - bx) * SUB);
        let (dx1, dy1) = (-(ay - cy) * SUB, (ax - cx) * SUB);
        let (dx2, dy2) = (-(by - ay) * SUB, (bx - ax) * SUB);

        let col = |c: Color32| [c.r() as f32, c.g() as f32, c.b() as f32, c.a() as f32];
        let (ca, cb, cc) = (col(va.color), col(vb.color), col(vc.color));
        let flat_color = va.color == vb.color && vb.color == vc.color;
        let flat_uv = va.uv == vb.uv && vb.uv == vc.uv;
        let n = maxx - minx;
        // Caso más común (rellenos y bordes sólidos): un solo color por triángulo
        let flat = if flat_color && flat_uv {
            let t = tex.sample(va.uv.x, va.uv.y);
            let px = |i: usize| ((ca[i] * t[i] / 255.0).round() as u32).min(255);
            let (r, g, b, a) = (px(0), px(1), px(2), px(3));
            if r == 0 && g == 0 && b == 0 && a == 0 {
                return;
            }
            Some((r, g, b, a))
        } else {
            None
        };
        let inv = 1.0 / area as f32;
        let k = 1.0 / 255.0;

        for y in miny..maxy {
            // tramo de la fila donde las tres aristas dan "dentro"
            let (l0, h0) = span(row0, dx0, bias0, n);
            let (l1, h1) = span(row1, dx1, bias1, n);
            let (l2, h2) = span(row2, dx2, bias2, n);
            let lo = l0.max(l1).max(l2);
            let hi = h0.min(h1).min(h2);
            if lo < hi {
                let line = y as usize * w + minx as usize;
                let row = &mut buf[line + lo as usize..line + hi as usize];
                if let Some((r, g, b, a)) = flat {
                    if a == 255 {
                        row.fill(r << 16 | g << 8 | b);
                    } else {
                        // dos canales por multiplicación (rojo y azul juntos), /255 aproximado
                        let ia = 255 - a;
                        let src = r << 16 | g << 8 | b;
                        for d in row.iter_mut() {
                            let mut rb = (*d & 0x00ff_00ff) * ia + 0x0080_0080;
                            rb = ((rb + ((rb >> 8) & 0x00ff_00ff)) >> 8) & 0x00ff_00ff;
                            let mut gg = (*d & 0x0000_ff00) * ia + 0x0000_8000;
                            gg = ((gg + ((gg >> 8) & 0x0000_ff00)) >> 8) & 0x0000_ff00;
                            *d = src + rb + gg;
                        }
                    }
                } else {
                    let (mut w0, mut w1) = (row0 + dx0 * lo, row1 + dx1 * lo);
                    for d in row.iter_mut() {
                        let (b0, b1) = (w0 as f32 * inv, w1 as f32 * inv);
                        let b2 = 1.0 - b0 - b1;
                        let c = if flat_color {
                            ca
                        } else {
                            let mut o = [0.0; 4];
                            for i in 0..4 {
                                o[i] = ca[i] * b0 + cb[i] * b1 + cc[i] * b2;
                            }
                            o
                        };
                        let t = if flat_uv {
                            tex.sample(va.uv.x, va.uv.y)
                        } else {
                            let u = va.uv.x * b0 + vb.uv.x * b1 + vc.uv.x * b2;
                            let v = va.uv.y * b0 + vb.uv.y * b1 + vc.uv.y * b2;
                            tex.sample(u, v)
                        };
                        let sa = c[3] * t[3] * k;
                        if sa > 0.0 {
                            let inv_a = 1.0 - sa * k;
                            let dr = ((*d >> 16) & 0xff) as f32;
                            let dg = ((*d >> 8) & 0xff) as f32;
                            let db = (*d & 0xff) as f32;
                            let r = (c[0] * t[0] * k + dr * inv_a).min(255.0) as u32;
                            let g = (c[1] * t[1] * k + dg * inv_a).min(255.0) as u32;
                            let b = (c[2] * t[2] * k + db * inv_a).min(255.0) as u32;
                            *d = r << 16 | g << 8 | b;
                        }
                        w0 += dx0;
                        w1 += dx1;
                    }
                }
            }
            row0 += dy0;
            row1 += dy1;
            row2 += dy2;
        }
    }
}

/// Píxeles k en 0..n (desde el borde izquierdo de la caja) con w + dx·k >= bias.
#[inline]
fn span(w: i64, dx: i64, bias: i64, n: i64) -> (i64, i64) {
    if dx == 0 {
        return if w >= bias { (0, n) } else { (0, 0) };
    }
    if dx > 0 {
        // k >= techo((bias - w) / dx)
        let lo = -((w - bias).div_euclid(dx));
        (lo.max(0), n)
    } else {
        // k <= suelo((w - bias) / -dx)
        let hi = (w - bias).div_euclid(-dx) + 1;
        (0, hi.min(n))
    }
}

// ------------------------------------------------------------------- bucle
enum UserEvent {
    Repaint(Instant),
}

/// Pide un fotograma desde otro hilo. A diferencia de `Context::request_repaint`
/// (que, llamado desde fuera, fuerza una pasada extra), aquí se pinta una vez.
#[derive(Clone)]
pub struct Waker(EventLoopProxy<UserEvent>);

impl Waker {
    pub fn wake(&self) {
        let _ = self.0.send_event(UserEvent::Repaint(Instant::now()));
    }
}

struct Runner {
    ctx: egui::Context,
    app: Box<dyn SoftApp>,
    builder: egui::ViewportBuilder,
    window: Option<Arc<Window>>,
    state: Option<egui_winit::State>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    raster: Raster,
    info: egui::ViewportInfo,
    repaint_at: Option<Instant>,
    error: Option<String>,
    /// ZENDOCK_PERF=1: tiempo de rasterizado de cada fotograma por stderr
    perf: bool,
    /// Tamaño de búfer más grande que ha pedido la superficie actual
    pool_max: u64,
    started: Instant,
    pointer: Option<(f32, f32, bool, u8)>,
}

/// ZENDOCK_PUNTERO=x,y[,clic]: puntero sintético a los 2 s (para capturar
/// menús y tooltips en pruebas sin tocar el ratón).
fn inject_pointer(
    pointer: &mut Option<(f32, f32, bool, u8)>,
    started: Instant,
    ctx: &egui::Context,
    raw: &mut egui::RawInput,
) {
    let Some((x, y, click, stage)) = pointer.as_mut() else { return };
    let pos = egui::pos2(*x, *y);
    let t = started.elapsed().as_secs_f32();
    let press = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    };
    match *stage {
        0 if t > 2.0 => {
            raw.events.push(egui::Event::PointerMoved(pos));
            *stage = if *click { 1 } else { 3 };
        }
        1 if t > 2.2 => {
            raw.events.push(egui::Event::PointerMoved(pos));
            raw.events.push(press(true));
            *stage = 2;
        }
        2 => {
            raw.events.push(press(false));
            *stage = 3;
        }
        _ => {}
    }
    if *stage < 3 {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
}

impl Runner {
    fn paint(&mut self, el: &ActiveEventLoop) {
        let (Some(window), Some(state), Some(surface)) =
            (self.window.clone(), self.state.as_mut(), self.surface.as_mut())
        else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        // softbuffer agranda sus búferes (a potencias de 2) pero nunca los
        // encoge: si la ventana se reduce mucho, se crea la superficie de nuevo
        let pool = ((size.width * size.height * 4) as u64).next_power_of_two();
        if pool * 2 <= self.pool_max
            && let (Some(ctx), Some(win)) = (self.context.as_ref(), self.window.clone())
            && let Ok(s) = softbuffer::Surface::new(ctx, win)
        {
            *surface = s;
            self.pool_max = 0;
        }
        self.pool_max = self.pool_max.max(pool);
        egui_winit::update_viewport_info(&mut self.info, &self.ctx, &window, false);
        let mut raw = state.take_egui_input(&window);
        raw.viewports.insert(ViewportId::ROOT, self.info.clone());
        inject_pointer(&mut self.pointer, self.started, &self.ctx, &mut raw);
        let app = &mut self.app;
        let out = self.ctx.run_ui(raw, |ui| app.ui(ui));
        if self.perf {
            let causes = self.ctx.repaint_causes();
            if !causes.is_empty() {
                eprintln!("  causas: {causes:?}");
            }
        }
        state.handle_platform_output_with_event_loop(&window, el, out.platform_output);
        let mut actions = Vec::new();
        if let Some(vo) = out.viewport_output.get(&ViewportId::ROOT) {
            egui_winit::process_viewport_commands(
                &self.ctx,
                &mut self.info,
                vo.commands.clone(),
                &window,
                &mut actions,
            );
        }
        self.raster.set_textures(&out.textures_delta);
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else { return };
        let (wu, hu) = (w.get() as usize, h.get() as usize);
        let t0 = Instant::now();
        self.raster.render(&mut buf, wu, hu, &prims, out.pixels_per_point, self.app.clear_color());
        if self.perf {
            let tris: usize = prims
                .iter()
                .map(|p| match &p.primitive {
                    Primitive::Mesh(m) => m.indices.len() / 3,
                    _ => 0,
                })
                .sum();
            eprintln!(
                "{:.3} fotograma {wu}x{hu}: {tris} triángulos, rasterizado en {:.2} ms",
                self.started.elapsed().as_secs_f64(),
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        for a in actions {
            if let egui_winit::ActionRequested::Screenshot(cb) = a {
                let pixels = buf[..wu * hu]
                    .iter()
                    .map(|p| Color32::from_rgb((p >> 16) as u8, (p >> 8) as u8, *p as u8))
                    .collect();
                cb.complete(Arc::new(egui::ColorImage::new([wu, hu], pixels)));
            }
        }
        window.pre_present_notify();
        let _ = buf.present();
        self.raster.free_textures(&out.textures_delta);
        if self.info.close_requested() {
            self.app.on_exit();
            el.exit();
        }
    }
}

impl ApplicationHandler<UserEvent> for Runner {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = egui_winit::create_winit_window_attributes(&self.ctx, self.builder.clone());
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.error = Some(e.to_string());
                el.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(e.to_string());
                el.exit();
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(e.to_string());
                el.exit();
                return;
            }
        };
        self.state = Some(egui_winit::State::new(
            self.ctx.clone(),
            ViewportId::ROOT,
            el,
            Some(window.scale_factor() as f32),
            None,
            Some(8192),
        ));
        egui_winit::update_viewport_info(&mut self.info, &self.ctx, &window, true);
        self.context = Some(context);
        self.surface = Some(surface);
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else { return };
        match &event {
            WindowEvent::CloseRequested => {
                self.app.on_exit();
                el.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                self.paint(el);
                return;
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => window.request_redraw(),
            _ => {}
        }
        if let Some(state) = self.state.as_mut()
            && state.on_window_event(&window, &event).repaint
        {
            window.request_redraw();
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, ev: UserEvent) {
        let UserEvent::Repaint(at) = ev;
        if self.perf {
            let now = Instant::now();
            eprintln!(
                "{:.3} repintar dentro de {:.3} s",
                self.started.elapsed().as_secs_f64(),
                at.saturating_duration_since(now).as_secs_f64()
            );
        }
        self.repaint_at = Some(self.repaint_at.map_or(at, |t| t.min(at)));
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        match self.repaint_at {
            Some(t) if t <= Instant::now() => {
                self.repaint_at = None;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Abre la ventana y corre el bucle hasta que se cierre.
pub fn run(
    builder: egui::ViewportBuilder,
    make: impl FnOnce(&egui::Context, Waker) -> Box<dyn SoftApp>,
) -> Result<(), String> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| e.to_string())?;
    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();
    let waker = Waker(proxy.clone());
    let ctx = egui::Context::default();
    ctx.set_request_repaint_callback(move |info| {
        let _ = proxy.send_event(UserEvent::Repaint(Instant::now() + info.delay));
    });
    let app = make(&ctx, waker);
    let mut runner = Runner {
        ctx,
        app,
        builder,
        window: None,
        state: None,
        context: None,
        surface: None,
        raster: Raster::default(),
        info: egui::ViewportInfo::default(),
        repaint_at: None,
        error: None,
        perf: std::env::var_os("ZENDOCK_PERF").is_some(),
        pool_max: 0,
        started: Instant::now(),
        pointer: std::env::var("ZENDOCK_PUNTERO").ok().and_then(|v| {
            let mut it = v.split(',');
            let x = it.next()?.trim().parse().ok()?;
            let y = it.next()?.trim().parse().ok()?;
            Some((x, y, it.next().is_some_and(|c| c.trim() == "clic"), 0))
        }),
    };
    event_loop.run_app(&mut runner).map_err(|e| e.to_string())?;
    match runner.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
