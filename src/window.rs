//! Process-level helpers for the preview window: pin state, a hang-proof exit, a watchdog (Windows).

use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::archive;

static PINNED: AtomicBool = AtomicBool::new(false);
static STICKY: AtomicBool = AtomicBool::new(false);

pub fn is_pinned() -> bool {
    PINNED.load(Ordering::Relaxed)
}

/// "Always pin" mode: previews start pinned and behave like Quick Look (Space/Esc close them).
pub fn set_sticky(on: bool) {
    STICKY.store(on, Ordering::Relaxed);
}

/// In a manually pinned window Space plays/pauses media (it doesn't close the window).
pub fn space_plays_media() -> bool {
    is_pinned() && !STICKY.load(Ordering::Relaxed)
}

/// The tray looks for this file to know a preview was pinned by hand and must not be replaced or killed.
pub fn pin_marker(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("quicklook-rs-pinned-{pid}"))
}

/// `leave_alone`: write the marker so the tray keeps this window when the next preview opens.
pub fn set_pinned(on: bool, leave_alone: bool) {
    PINNED.store(on, Ordering::Relaxed);
    let marker = pin_marker(std::process::id());
    if on && leave_alone {
        let _ = std::fs::write(marker, b"pinned");
    } else {
        let _ = std::fs::remove_file(marker);
    }
}

/// Clean up, then end the process immediately. Normal shutdown (DLL detach, COM/WinRT threads,
/// the GPU context, libmpv) is what could leave a window "Not responding", so we skip it.
pub fn fast_exit() -> ! {
    archive::cleanup_current();
    let _ = std::fs::remove_file(pin_marker(std::process::id()));
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        let _ = TerminateProcess(GetCurrentProcess(), 0);
    }
    std::process::exit(0)
}

#[cfg(windows)]
mod win {
    use std::{
        ffi::c_void,
        sync::atomic::{AtomicIsize, Ordering},
        time::Duration,
    };

    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::IsHungAppWindow};

    static WINDOW: AtomicIsize = AtomicIsize::new(0);

    pub fn set_window(hwnd: isize) {
        WINDOW.store(hwnd, Ordering::Relaxed);
    }

    pub fn window_hwnd() -> isize {
        WINDOW.load(Ordering::Relaxed)
    }

    /// Windows 11: round the corners of the borderless window. Silently does nothing on older Windows.
    pub fn round_corners(hwnd: isize) {
        use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
        let preference = DWMWCP_ROUND;
        unsafe {
            let _ = DwmSetWindowAttribute(
                HWND(hwnd as *mut c_void),
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &preference as *const _ as *const c_void,
                std::mem::size_of_val(&preference) as u32,
            );
        }
    }

    /// Safety net: if our window stops responding for a few seconds, whatever the cause, end the process.
    pub fn start_watchdog() {
        std::thread::spawn(|| {
            let mut hung_checks = 0;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let raw = WINDOW.load(Ordering::Relaxed);
                if raw == 0 {
                    continue;
                }
                let hung = unsafe { IsHungAppWindow(HWND(raw as *mut c_void)).as_bool() };
                hung_checks = if hung { hung_checks + 1 } else { 0 };
                if hung_checks >= 2 {
                    super::fast_exit();
                }
            }
        });
    }
}

#[cfg(windows)]
pub use win::{round_corners, set_window, start_watchdog, window_hwnd};

#[cfg(not(windows))]
pub fn start_watchdog() {}
