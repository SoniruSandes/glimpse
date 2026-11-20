//! Starts the preview window as a child process (own event loop, can't leak or hang the tray).
//! A preview pinned by hand is left alone: the next preview opens next to it.
//! In "Always pin" mode previews start pinned but are still replaced/toggled by the next Space.

use std::{
    path::PathBuf,
    process::{Child, Command},
    sync::Mutex,
};

use crate::{archive, settings, window};

static CURRENT: Mutex<Option<(Child, PathBuf)>> = Mutex::new(None);

fn kill(child: &mut Child) {
    let pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    // A killed preview can't clean up its own temp files (archive entries).
    archive::cleanup_pid(pid);
}

fn is_hand_pinned(child: &Child) -> bool {
    window::pin_marker(child.id()).exists()
}

/// Open a preview for `path`. A preview that is already open is closed first (unless you pinned it by hand),
/// and if it was for the same file we stop there (Space toggles).
pub fn toggle(path: PathBuf, via_shortcut: bool) {
    let Ok(mut current) = CURRENT.lock() else { return };

    if let Some((mut child, previous)) = current.take() {
        if matches!(child.try_wait(), Ok(None)) {
            if is_hand_pinned(&child) {
                drop(child); // detach: it keeps running as a normal window
            } else {
                kill(&mut child);
                if previous == path {
                    return;
                }
            }
        }
    }

    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = Command::new(exe);
    if settings::always_pin() {
        command.arg("--always-pin");
    }
    if via_shortcut {
        command.arg("--via-shortcut");
    }
    command.arg("--preview").arg(&path);
    if let Ok(child) = command.spawn() {
        #[cfg(windows)]
        unsafe {
            // Let the new process take keyboard focus.
            let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id());
        }
        *current = Some((child, path));
    }
}

/// Close the current preview (unless pinned by hand). Used on quit and by D-Bus "Close".
pub fn close_current() {
    if let Ok(mut current) = CURRENT.lock() {
        if let Some((mut child, _)) = current.take() {
            if !is_hand_pinned(&child) {
                kill(&mut child);
            }
        }
    }
}
