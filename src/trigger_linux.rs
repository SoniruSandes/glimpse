//! Linux trigger: GNOME Files (Nautilus) shows a preview by calling the D-Bus method
//! org.gnome.NautilusPreviewer.ShowFile when you press Space (the same interface GNOME Sushi implements).
//! If Sushi is installed and running it owns that name; stop it (or uninstall it) to use this app instead.
//! Other file managers: run `quicklook <path>` from a custom action or a keyboard shortcut.

use std::path::PathBuf;

struct Previewer;

#[zbus::interface(name = "org.gnome.NautilusPreviewer")]
impl Previewer {
    fn show_file(&self, uri: &str, _parent_xid: i32, _close_if_already_visible: bool) {
        if !crate::settings::enabled() {
            return;
        }
        if let Some(path) = uri_to_path(uri) {
            crate::launcher::toggle(path, false);
        }
    }

    fn close(&self) {
        crate::launcher::close_current();
    }
}

pub fn start() {
    std::thread::spawn(|| {
        let connection = (|| -> zbus::Result<zbus::blocking::Connection> {
            zbus::blocking::connection::Builder::session()?
                .name("org.gnome.NautilusPreviewer")?
                .serve_at("/org/gnome/NautilusPreviewer", Previewer)?
                .build()
        })();
        if let Ok(connection) = connection {
            let _keep_alive = connection;
            loop {
                std::thread::park();
            }
        }
    });
}

/// file:///home/me/My%20Pictures/a.png -> /home/me/My Pictures/a.png
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}
