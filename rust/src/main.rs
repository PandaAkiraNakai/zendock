//! ZenDock: los contenedores Docker en la bandeja del sistema, con una ventana
//! aparte que solo existe mientras está abierta.
//!
//! - `zendock --tray`: demonio de la bandeja (StatusNotifierItem, sin toolkit).
//! - `zendock`: la ventana (egui). Si ya está abierta, la trae al frente.

mod apps;
mod config;
mod docker;
mod engine;
mod gate;
mod icon;
mod model;
mod notify;
mod probe;
mod soft;
mod tray;
mod util;
mod window;

const AYUDA: &str = "\
uso: zendock [opción]

  (sin opción)        abre la ventana (o trae al frente la que ya está abierta)
  --tray              arranca el icono de la bandeja, en segundo plano
  --write-icon RUTA   guarda el icono en PNG (256 px), para el instalador
  -h, --help          muestra esta ayuda
  -V, --version       muestra la versión";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => window::run(),
        Some("--tray") => tray::run(),
        Some("--write-icon") => {
            let Some(path) = args.get(1) else {
                eprintln!("falta la ruta del PNG");
                std::process::exit(2);
            };
            if let Err(e) = std::fs::write(path, icon::png(model::Accent::Blue, 256)) {
                eprintln!("no se pudo escribir {path}: {e}");
                std::process::exit(1);
            }
        }
        Some("-h" | "--help") => println!("{AYUDA}"),
        Some("-V" | "--version") => println!("zendock {}", env!("CARGO_PKG_VERSION")),
        Some(other) => {
            eprintln!("opción desconocida: {other}\n\n{AYUDA}");
            std::process::exit(2);
        }
    }
}
