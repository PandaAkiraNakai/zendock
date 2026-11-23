//! Configuración: `enlaces.json` (webs y apps fijadas a mano) y las preferencias
//! en `zendock.conf`, con el mismo formato que dejaba QSettings en la versión
//! de Python para no perderlas al cambiar.

use std::path::PathBuf;
use std::time::SystemTime;

use serde_json::{Map, Value};

use crate::util::config_dir;

/// enlaces.json: {"contenedor o proyecto": {"web": [...], "app": "id.desktop"}}
#[derive(Default)]
pub struct Links {
    mtime: Option<SystemTime>,
    pub data: Map<String, Value>,
    pub error: String,
}

impl Links {
    pub fn path() -> PathBuf {
        config_dir().join("enlaces.json")
    }

    /// Relee el fichero solo si cambió. Devuelve true si hubo cambios.
    pub fn reload(&mut self) -> bool {
        let path = Self::path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime.is_none() {
            let changed = self.mtime.is_some() || !self.error.is_empty();
            *self = Links::default();
            return changed;
        }
        if mtime == self.mtime {
            return false;
        }
        self.mtime = mtime;
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()));
        match parsed {
            Ok(Value::Object(m)) => {
                self.data = m;
                self.error.clear();
            }
            Ok(_) => {
                self.data.clear();
                self.error = format!("{}: tiene que ser un objeto", path.display());
            }
            Err(e) => {
                self.data.clear();
                self.error = format!("{}: {e}", path.display());
            }
        }
        true
    }

    pub fn entry(&self, key: &str, field: &str) -> &Value {
        self.data.get(key).and_then(|v| v.get(field)).unwrap_or(&Value::Null)
    }
}

/// Preferencias: avisos, solo_en_marcha y size, en [General] de zendock.conf.
pub struct Settings;

impl Settings {
    fn path() -> PathBuf {
        config_dir().join("zendock.conf")
    }

    fn read_raw(key: &str) -> Option<String> {
        let text = std::fs::read_to_string(Self::path()).ok()?;
        let mut in_general = false;
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with('[') {
                in_general = l == "[General]";
            } else if in_general
                && let Some((k, v)) = l.split_once('=')
                && k.trim() == key
            {
                return Some(v.trim().to_owned());
            }
        }
        None
    }

    fn write_raw(key: &str, value: &str) {
        let path = Self::path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let mut general = None;
        let mut done = false;
        let mut i = 0;
        while i < lines.len() {
            let l = lines[i].trim().to_owned();
            if l.starts_with('[') {
                if general.is_some() && !done {
                    break;
                }
                if l == "[General]" {
                    general = Some(i);
                }
            } else if general.is_some()
                && let Some((k, _)) = l.split_once('=')
                && k.trim() == key
            {
                lines[i] = format!("{key}={value}");
                done = true;
                break;
            }
            i += 1;
        }
        if !done {
            match general {
                Some(g) => lines.insert(g + 1, format!("{key}={value}")),
                None => {
                    lines.insert(0, String::new());
                    lines.insert(0, format!("{key}={value}"));
                    lines.insert(0, "[General]".into());
                }
            }
        }
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let _ = std::fs::create_dir_all(config_dir());
        let tmp = path.with_extension("conf.tmp");
        if std::fs::write(&tmp, lines.join("\n") + "\n").is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    pub fn get_bool(key: &str, default: bool) -> bool {
        match Self::read_raw(key).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => default,
        }
    }

    pub fn set_bool(key: &str, value: bool) {
        Self::write_raw(key, if value { "true" } else { "false" });
    }

    /// size=@Size(500 760)
    pub fn get_size() -> Option<(f32, f32)> {
        let raw = Self::read_raw("size")?;
        let inner = raw.strip_prefix("@Size(")?.strip_suffix(')')?;
        let mut it = inner.split_whitespace().map(|n| n.parse::<f32>().ok());
        let (w, h) = (it.next()??, it.next()??);
        (w >= 200.0 && h >= 200.0).then_some((w, h))
    }

    pub fn set_size(w: f32, h: f32) {
        Self::write_raw("size", &format!("@Size({} {})", w.round() as i64, h.round() as i64));
    }
}
