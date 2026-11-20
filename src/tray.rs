//! Tray icon, its small menu, and the platform event loop (Win32 messages / GTK).

use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};

use crate::{launcher, settings};

#[cfg(windows)]
const ENABLED_LABEL: &str = "Enabled (Space in Explorer / desktop)";
#[cfg(not(windows))]
const ENABLED_LABEL: &str = "Enabled (Space in GNOME Files)";

pub fn run() {
    if !is_first_instance() {
        return;
    }
    settings::load();

    #[cfg(target_os = "linux")]
    {
        if gtk::init().is_err() {
            return;
        }
        crate::trigger_linux::start();
    }
    #[cfg(windows)]
    let Ok(hook_handle) = crate::hook::install() else {
        return;
    };

    let menu = Menu::new();
    let enabled = CheckMenuItem::new(ENABLED_LABEL, true, settings::enabled(), None);
    let always_pin = CheckMenuItem::new("Always pin previews", true, settings::always_pin(), None);
    let startup = CheckMenuItem::new("Start at login", true, autostart_enabled(), None);
    let quit = MenuItem::new("Quit", true, None);
    let _ = menu.append_items(&[&enabled, &always_pin, &startup, &PredefinedMenuItem::separator(), &quit]);

    let (enabled_id, pin_id, startup_id, quit_id) =
        (enabled.id().clone(), always_pin.id().clone(), startup.id().clone(), quit.id().clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        // The check marks flip natively; keep our settings in step with them.
        if event.id == quit_id {
            quit_event_loop();
        } else if event.id == enabled_id {
            settings::set_enabled(!settings::enabled());
        } else if event.id == pin_id {
            settings::set_always_pin(!settings::always_pin());
        } else if event.id == startup_id {
            set_autostart(!autostart_enabled());
        }
    }));

    let Ok(_tray) = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Quick Look: press Space on a file")
        .with_icon(load_icon().unwrap_or_else(make_icon))
        .build()
    else {
        return;
    };

    event_loop();

    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(hook_handle);
    }
    launcher::close_current();
}

// ---- event loop ----

/// The keyboard hook and the tray icon both need this loop on the thread that created them.
#[cfg(windows)]
fn event_loop() {
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, TranslateMessage, MSG};
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(windows)]
fn quit_event_loop() {
    unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
}

#[cfg(target_os = "linux")]
fn event_loop() {
    gtk::main();
}

#[cfg(target_os = "linux")]
fn quit_event_loop() {
    gtk::main_quit();
}

// ---- single instance ----

#[cfg(windows)]
fn is_first_instance() -> bool {
    use windows::{
        core::w,
        Win32::{
            Foundation::{GetLastError, ERROR_ALREADY_EXISTS},
            System::Threading::CreateMutexW,
        },
    };
    unsafe {
        match CreateMutexW(None, true, w!("Local\\RustQuickLookTray")) {
            Ok(handle) => {
                let already = GetLastError() == ERROR_ALREADY_EXISTS;
                // Keep the mutex alive for the life of the process by intentionally ignoring it.
                let _ = handle;
                !already
            }
            Err(_) => true,
        }
    }
}

#[cfg(unix)]
fn is_first_instance() -> bool {
    use std::os::unix::net::{UnixListener, UnixStream};
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join("quicklook-rs.sock");
    if UnixStream::connect(&path).is_ok() {
        return false; // another tray is listening
    }
    let _ = std::fs::remove_file(&path); // stale socket from a crashed run
    if let Ok(listener) = UnixListener::bind(&path) {
        std::mem::forget(listener); // keep it open for the life of the process
    }
    true
}

// ---- start at login ----

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "RustQuickLook";

#[cfg(windows)]
fn autostart_enabled() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|key| key.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

#[cfg(windows)]
fn set_autostart(on: bool) {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY) else {
        return;
    };
    if on {
        if let Ok(exe) = std::env::current_exe() {
            let _ = key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()));
        }
    } else {
        let _ = key.delete_value(RUN_VALUE);
    }
}

#[cfg(not(windows))]
fn autostart_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("quicklook-rs.desktop"))
}

#[cfg(not(windows))]
fn autostart_enabled() -> bool {
    autostart_file().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(not(windows))]
fn set_autostart(on: bool) {
    let Some(path) = autostart_file() else { return };
    if on {
        if let (Some(dir), Ok(exe)) = (path.parent(), std::env::current_exe()) {
            let _ = std::fs::create_dir_all(dir);
            let entry = format!(
                "[Desktop Entry]\nType=Application\nName=Quick Look\nExec=\"{}\"\nX-GNOME-Autostart-enabled=true\n",
                exe.display()
            );
            let _ = std::fs::write(path, entry);
        }
    } else {
        let _ = std::fs::remove_file(path);
    }
}

// ---- icon ----

/// icons/tray.ico (embedded at build time). The largest frame of the ICO is used.
fn load_icon() -> Option<Icon> {
    let bytes = include_bytes!("../icons/tray.ico");
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Ico).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).ok()
}

/// Fallback: draws a 32x32 icon in code (blue rounded square, white page, spacebar), so no asset file is needed.
fn make_icon() -> Icon {
    const N: u32 = 32;
    let radius = 7.0_f32;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (cx, cy) = (fx.clamp(radius, N as f32 - radius), fy.clamp(radius, N as f32 - radius));
            let inside = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() <= radius;
            let page = (9..23).contains(&x) && (5..27).contains(&y);
            let bar = (11..21).contains(&x) && (20..24).contains(&y);
            let px: [u8; 4] = if !inside {
                [0, 0, 0, 0]
            } else if bar {
                [59, 130, 246, 255]
            } else if page {
                [255, 255, 255, 255]
            } else {
                [59, 130, 246, 255]
            };
            rgba.extend_from_slice(&px);
        }
    }
    Icon::from_rgba(rgba, N, N).expect("valid icon buffer")
}
