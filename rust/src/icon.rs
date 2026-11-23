//! El icono de ZenDock: tres contenedores apilados con sus nervios, dibujado a
//! mano (con suavizado por supermuestreo) para no depender de ningún tema.

use crate::model::Accent;

const BOXES: [(f32, f32); 3] = [(8.0, 52.0), (52.0, 52.0), (30.0, 18.0)];

fn in_rrect(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> bool {
    if px < x || py < y || px > x + w || py > y + h {
        return false;
    }
    let cx = px.clamp(x + r, x + w - r);
    let cy = py.clamp(y + r, y + h - r);
    let (dx, dy) = (px - cx, py - cy);
    dx * dx + dy * dy <= r * r
}

/// Cobertura (0..1) del dibujo en el punto (x, y) de un lienzo de 100×100.
fn covered(x: f32, y: f32) -> bool {
    for (bx, by) in BOXES {
        if in_rrect(x, y, bx, by, 40.0, 30.0, 4.0) {
            // los nervios de la chapa son huecos en el relleno
            for i in 0..3 {
                if in_rrect(x, y, bx + 8.0 + i as f32 * 10.0, by + 7.0, 4.0, 16.0, 1.5) {
                    return false;
                }
            }
            return true;
        }
    }
    false
}

/// RGBA sin premultiplicar, `size`×`size`.
pub fn rgba(accent: Accent, size: u32) -> Vec<u8> {
    let [r, g, b] = accent.rgb();
    let n = 4u32; // muestras por eje
    let scale = 100.0 / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..n {
                for sx in 0..n {
                    let x = (px as f32 + (sx as f32 + 0.5) / n as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / n as f32) * scale;
                    hits += covered(x, y) as u32;
                }
            }
            let a = (hits * 255 / (n * n)) as u8;
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    out
}

/// ARGB32 en orden de red, como lo pide StatusNotifierItem.
pub fn argb(accent: Accent, size: u32) -> Vec<u8> {
    let mut px = rgba(accent, size);
    for c in px.as_chunks_mut::<4>().0 {
        c.rotate_right(1);
    }
    px
}

pub fn png(accent: Accent, size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut w) = enc.write_header() {
            let _ = w.write_image_data(&rgba(accent, size));
        }
    }
    out
}

/// Lee un PNG como RGBA de 8 bits (para los iconos de las apps).
pub fn load_png(path: &std::path::Path) -> Option<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => {
            buf.as_chunks::<2>().0.iter().flat_map(|c| [c[0], c[0], c[0], c[1]]).collect()
        }
        _ => return None,
    };
    Some((w, h, rgba))
}
