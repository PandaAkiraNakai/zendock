//! Utilidades del sistema: rutas XDG, búsqueda en el PATH y lanzar programas
//! sueltos (navegador, terminal, portapapeles) sin quedarse esperándolos.

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => Path::new(&home()).join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("zendock")
}

pub fn data_home() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
}

pub fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

/// Igual que `shutil.which`: con una barra se comprueba la ruta tal cual.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let ok =
        |p: &Path| p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false);
    if name.is_empty() {
        return None;
    }
    if name.contains('/') {
        let p = PathBuf::from(name);
        return ok(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| ok(p))
}

/// Lanza un programa desligado de este proceso (su propio grupo, sin heredar la
/// salida). Un hilo lo recoge al terminar para no dejar zombis.
pub fn spawn_detached<I, S>(prog: &str, args: I, envs: &[(&str, String)]) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(prog);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let _ =
                std::thread::Builder::new().name("recoger".into()).stack_size(64 * 1024).spawn(move || {
                    let _ = child.wait();
                });
            true
        }
        Err(_) => false,
    }
}

/// Abre una URL o una carpeta con la aplicación predeterminada.
pub fn open_url(target: &str) -> bool {
    spawn_detached("xdg-open", [target], &[])
}

/// Copia al portapapeles con una herramienta externa: así el texto sigue
/// disponible aunque la ventana se cierre (en Wayland el portapapeles es del
/// proceso que lo puso).
pub fn copy_text(text: &str) -> bool {
    use std::io::Write;
    let candidates: &[(&str, &[&str])] = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["-b", "-i"])]
    } else {
        &[("xclip", &["-selection", "clipboard"]), ("xsel", &["-b", "-i"]), ("wl-copy", &[])]
    };
    for (prog, args) in candidates {
        if which(prog).is_none() {
            continue;
        }
        let child = Command::new(prog)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
    }
    false
}

/// Abre `args` en un terminal que se queda abierto al terminar.
pub fn in_terminal(title: &str, args: &[&str]) -> bool {
    if which("konsole").is_some() {
        let tab = format!("tabtitle={title}");
        let mut a = vec!["-p", tab.as_str(), "--hold", "-e"];
        a.extend_from_slice(args);
        return spawn_detached("konsole", a, &[]);
    }
    if which("kitty").is_some() {
        let mut a = vec!["--hold", "--title", title];
        a.extend_from_slice(args);
        return spawn_detached("kitty", a, &[]);
    }
    for term in ["x-terminal-emulator", "kgx", "gnome-terminal", "xterm"] {
        if which(term).is_some() {
            let mut a = vec!["-e"];
            a.extend_from_slice(args);
            return spawn_detached(term, a, &[]);
        }
    }
    false
}

/// Codifica para la query de una URL (lo justo para los filtros de Docker).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
