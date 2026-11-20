//! Low-level keyboard hook. Swallows Space only when an Explorer file list or the desktop icons
//! have focus, then asks a worker thread (which owns COM) to find the selection and open the preview.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        OnceLock,
    },
    thread,
};

use windows::Win32::{
    Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
    System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT, VK_SPACE},
        WindowsAndMessaging::{
            CallNextHookEx, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
            SetWindowsHookExW, GUITHREADINFO, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, WH_KEYBOARD_LL,
            WM_KEYDOWN, WM_KEYUP,
        },
    },
};

use crate::{
    explorer::{self, Target},
    launcher,
};

/// True between a swallowed Space key-down and its key-up (also swallows auto-repeat).
static SWALLOWED: AtomicBool = AtomicBool::new(false);
static TX: OnceLock<Sender<Target>> = OnceLock::new();

pub fn install() -> windows::core::Result<HHOOK> {
    let (tx, rx) = mpsc::channel::<Target>();
    let _ = TX.set(tx);

    thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        for target in rx {
            if let Some((path, via_shortcut)) = explorer::selected_file(target) {
                launcher::toggle(path, via_shortcut);
            }
        }
    });

    unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), HINSTANCE::default(), 0) }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && crate::settings::enabled() {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
        if kb.vkCode == VK_SPACE.0 as u32 && !injected {
            match wparam.0 as u32 {
                WM_KEYDOWN => {
                    if SWALLOWED.load(Ordering::SeqCst) {
                        return LRESULT(1); // auto-repeat
                    }
                    if let Some(target) = target_with_focus() {
                        SWALLOWED.store(true, Ordering::SeqCst);
                        if let Some(tx) = TX.get() {
                            let _ = tx.send(target);
                        }
                        return LRESULT(1);
                    }
                }
                WM_KEYUP => {
                    if SWALLOWED.swap(false, Ordering::SeqCst) {
                        return LRESULT(1);
                    }
                }
                _ => {}
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

/// Returns where the preview should look for a selection if the foreground window is an Explorer
/// folder (file list focused) or the desktop (icon list focused) and no modifier is held.
/// A rename box, search box or address bar has a different focus window, so those are left alone.
/// Must stay cheap: it runs inside the hook callback.
unsafe fn target_with_focus() -> Option<Target> {
    for vk in [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN] {
        if GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 {
            return None;
        }
    }

    let foreground = GetForegroundWindow();
    if foreground.0.is_null() {
        return None;
    }
    let (target, list_class) = match class_name(foreground).as_str() {
        "CabinetWClass" => (Target::Folder(foreground.0 as isize), "DirectUIHWND"),
        "Progman" | "WorkerW" => (Target::Desktop, "SysListView32"),
        _ => return None,
    };

    let thread_id = GetWindowThreadProcessId(foreground, None);
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    GetGUIThreadInfo(thread_id, &mut info).ok()?;
    if info.hwndFocus.0.is_null() || class_name(info.hwndFocus) != list_class {
        return None;
    }
    Some(target)
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}
