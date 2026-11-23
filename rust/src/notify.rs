//! Notificaciones de escritorio por org.freedesktop.Notifications (D-Bus).

use std::collections::HashMap;
use std::sync::Mutex;

use zbus::blocking::Connection;
use zbus::zvariant::Value;

static CONN: Mutex<Option<Connection>> = Mutex::new(None);

pub fn send(summary: &str, body: &str) {
    let mut guard = CONN.lock().unwrap();
    if guard.is_none() {
        *guard = Connection::session().ok();
    }
    let Some(conn) = guard.as_ref() else { return };
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("desktop-entry", Value::from("zendock"));
    hints.insert("urgency", Value::U8(1));
    let res = conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &("ZenDock", 0u32, "dialog-warning", summary, body, Vec::<&str>::new(), hints, 8000i32),
    );
    if res.is_err() {
        // la conexión pudo caerse: la próxima vez se abre otra
        *guard = None;
    }
}
