//! Apps de escritorio (.desktop) que acompañan a un proyecto: se descubren
//! solas por nombre o por carpeta, o se fijan con `zendock.app`/enlaces.json.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::config::Links;
use crate::util::{self, home, which};

#[derive(Debug)]
pub struct DesktopApp {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub path: PathBuf,
    /// Ejecutable ya resuelto (sigue enlaces simbólicos)
    pub target: String,
    /// Contenido del ejecutable si es un script de $HOME
    pub text: String,
}

pub fn app_dirs() -> Vec<PathBuf> {
    vec![
        util::data_home().join("applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/usr/share/applications"),
    ]
}

/// Troceado al estilo de `shlex.split` (comillas simples, dobles y barra invertida).
pub fn shlex_split(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                have = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => cur.push(ch),
                    }
                }
            }
            '"' => {
                have = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some('"' | '\\' | '$' | '`') => cur.push(chars.next()?),
                            Some('\n') => {
                                chars.next();
                            }
                            _ => cur.push('\\'),
                        },
                        ch => cur.push(ch),
                    }
                }
            }
            '\\' => {
                have = true;
                match chars.next() {
                    Some('\n') => {}
                    Some(ch) => cur.push(ch),
                    None => return None,
                }
            }
            c if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        out.push(cur);
    }
    Some(out)
}

/// Claves del grupo [Desktop Entry]; la última aparición gana, como configparser.
fn desktop_entry(text: &str) -> Option<Vec<(String, String)>> {
    let mut in_entry = false;
    let mut seen = false;
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') && l.ends_with(']') {
            in_entry = l == "[Desktop Entry]";
            seen |= in_entry;
            continue;
        }
        if !in_entry || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            let k = k.trim().to_owned();
            let v = v.trim().to_owned();
            if let Some(slot) = out.iter_mut().find(|(kk, _)| *kk == k) {
                slot.1 = v;
            } else {
                out.push((k, v));
            }
        }
    }
    seen.then_some(out)
}

fn get<'a>(entry: &'a [(String, String)], key: &str) -> Option<&'a str> {
    entry.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

pub fn scan_apps() -> Vec<Arc<DesktopApp>> {
    let home_prefix = format!("{}/", home());
    let mut apps = Vec::new();
    let mut seen = HashSet::new();
    for folder in app_dirs() {
        let Ok(rd) = std::fs::read_dir(&folder) else { continue };
        let mut paths: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "desktop"))
            .collect();
        paths.sort();
        for path in paths {
            let fname = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if !seen.insert(fname) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(text) = String::from_utf8(bytes) else { continue };
            let Some(entry) = desktop_entry(&text) else { continue };
            if get(&entry, "Type").unwrap_or("Application") != "Application"
                || get(&entry, "Hidden").is_some_and(|h| h.eq_ignore_ascii_case("true"))
            {
                continue;
            }
            let exe = shlex_split(get(&entry, "Exec").unwrap_or(""))
                .and_then(|v| v.into_iter().next())
                .unwrap_or_default();
            let target = if exe.is_empty() {
                String::new()
            } else {
                let found = which(&exe).unwrap_or_else(|| PathBuf::from(&exe));
                let abs = if found.is_absolute() {
                    found
                } else {
                    std::env::current_dir().unwrap_or_default().join(found)
                };
                std::fs::canonicalize(&abs).unwrap_or(abs).to_string_lossy().into_owned()
            };
            let mut script = String::new();
            if target.starts_with(&home_prefix)
                && let Ok(f) = std::fs::File::open(&target)
            {
                let mut head = Vec::new();
                if f.take(200_000).read_to_end(&mut head).is_ok()
                    && !head[..head.len().min(1024)].contains(&0)
                {
                    script = String::from_utf8_lossy(&head).into_owned();
                }
            }
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let name = get(&entry, "Name[es]")
                .or_else(|| get(&entry, "Name"))
                .filter(|n| !n.is_empty())
                .unwrap_or(&stem)
                .to_owned();
            apps.push(Arc::new(DesktopApp {
                id: stem,
                name,
                icon: get(&entry, "Icon").unwrap_or("").to_owned(),
                path,
                target,
                text: script,
            }));
        }
    }
    apps
}

/// `workdir` seguido de algo que no sea [\w.-] (o el final del texto).
fn mentions_dir(text: &str, workdir: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text[from..].find(workdir) {
        let end = from + i + workdir.len();
        match text[end..].chars().next() {
            None => return true,
            Some(c) if !(c.is_alphanumeric() || c == '_' || c == '.' || c == '-') => return true,
            _ => {}
        }
        from = from + i + workdir.len().max(1);
        if from >= text.len() {
            break;
        }
    }
    false
}

/// Apps de escritorio de un proyecto (o de un contenedor suelto).
pub fn match_apps(
    apps: &[Arc<DesktopApp>],
    key: &str,
    workdir: &str,
    links: &Links,
    label: &str,
) -> Vec<Arc<DesktopApp>> {
    let mut wanted: Vec<String> =
        label.split(',').map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned).collect();
    match links.entry(key, "app") {
        Value::String(s) if !s.is_empty() => wanted.push(s.clone()),
        Value::Array(a) => wanted.extend(a.iter().filter_map(|v| v.as_str().map(str::to_owned))),
        _ => {}
    }
    if !wanted.is_empty() {
        return wanted
            .iter()
            .map(|w| w.trim_end_matches(".desktop"))
            .flat_map(|id| apps.iter().filter(move |a| a.id == id).cloned())
            .collect();
    }
    let dir_prefix = format!("{workdir}/");
    apps.iter()
        .filter(|a| a.id != "zendock")
        .filter(|a| {
            a.id == key
                || (!workdir.is_empty()
                    && (a.target.starts_with(&dir_prefix) || mentions_dir(&a.text, workdir)))
        })
        .cloned()
        .collect()
}

pub fn launch_app(app: &DesktopApp) -> bool {
    let in_std_dir = app.path.parent().is_some_and(|p| app_dirs().iter().any(|d| d == p));
    if in_std_dir && which("gtk-launch").is_some() {
        return util::spawn_detached("gtk-launch", [&app.id], &[]);
    }
    let Ok(text) = std::fs::read_to_string(&app.path) else { return false };
    let Some(entry) = desktop_entry(&text) else { return false };
    let Some(args) = shlex_split(get(&entry, "Exec").unwrap_or("")) else { return false };
    let args: Vec<String> = args
        .into_iter()
        .filter(|a| !(a.len() == 2 && a.starts_with('%') && a.as_bytes()[1].is_ascii_alphabetic()))
        .collect();
    match args.split_first() {
        Some((prog, rest)) => util::spawn_detached(prog, rest, &[]),
        None => false,
    }
}

/// Ruta de un icono PNG para la app: absoluta, o buscada en hicolor y pixmaps.
/// (Los temas solo con SVG no se cubren.)
pub fn icon_png_path(icon: &str) -> Option<PathBuf> {
    if icon.is_empty() {
        return None;
    }
    let p = Path::new(icon);
    if p.is_absolute() {
        return (p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) && p.is_file())
            .then(|| p.to_path_buf());
    }
    let h = home();
    let bases = [
        util::data_home().join("icons"),
        PathBuf::from(format!("{h}/.icons")),
        PathBuf::from("/usr/local/share/icons"),
        PathBuf::from("/usr/share/icons"),
    ];
    for theme in ["hicolor", "breeze-dark", "breeze", "Adwaita"] {
        for size in ["32x32", "48x48", "24x24", "64x64", "22x22", "128x128", "256x256", "16x16"] {
            for base in &bases {
                let f = base.join(theme).join(size).join("apps").join(format!("{icon}.png"));
                if f.is_file() {
                    return Some(f);
                }
            }
        }
    }
    let f = PathBuf::from(format!("/usr/share/pixmaps/{icon}.png"));
    f.is_file().then_some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn troceado() {
        assert_eq!(
            shlex_split(r#"env A=1 "/ruta con espacio/app" --x='y z' %U"#).unwrap(),
            vec!["env", "A=1", "/ruta con espacio/app", "--x=y z", "%U"]
        );
        assert!(shlex_split("\"sin cerrar").is_none());
    }

    #[test]
    fn carpeta_mencionada() {
        assert!(mentions_dir("cd /home/u/docker/mando && run", "/home/u/docker/mando"));
        assert!(!mentions_dir("cd /home/u/docker/mando-x", "/home/u/docker/mando"));
        assert!(mentions_dir("x /home/u/docker/mando-x /home/u/docker/mando/", "/home/u/docker/mando"));
    }
}
