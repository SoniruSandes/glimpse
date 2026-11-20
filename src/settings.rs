//! Settings (a tiny key=value file) and per-user folders, for Windows and Linux.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

static ENABLED: AtomicBool = AtomicBool::new(true);
static ALWAYS_PIN: AtomicBool = AtomicBool::new(false);

/// %APPDATA%\quicklook-rs  or  $XDG_CONFIG_HOME/quicklook-rs (~/.config/quicklook-rs)
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let dir = base?.join("quicklook-rs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// %APPDATA%\quicklook-rs  or  $XDG_DATA_HOME/quicklook-rs (~/.local/share/quicklook-rs)
pub fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dir = base?.join("quicklook-rs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn file() -> Option<PathBuf> {
    Some(config_dir()?.join("settings.ini"))
}

pub fn load() {
    let Some(path) = file() else { return };
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let on = value.trim() == "true";
            match key.trim() {
                "always_pin" => ALWAYS_PIN.store(on, Ordering::Relaxed),
                "enabled" => ENABLED.store(on, Ordering::Relaxed),
                _ => {}
            }
        }
    }
}

fn save() {
    if let Some(path) = file() {
        let text = format!("always_pin={}\nenabled={}\n", always_pin(), enabled());
        let _ = std::fs::write(path, text);
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    save();
}

pub fn always_pin() -> bool {
    ALWAYS_PIN.load(Ordering::Relaxed)
}

pub fn set_always_pin(on: bool) {
    ALWAYS_PIN.store(on, Ordering::Relaxed);
    save();
}
