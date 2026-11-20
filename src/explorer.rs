//! Ask Explorer (via COM) which file is selected: in an Explorer window or on the desktop.

use std::{
    ffi::c_void,
    path::{Path, PathBuf},
};

use windows::{
    core::{Interface, HSTRING, VARIANT},
    Win32::{
        Foundation::HWND,
        Storage::FileSystem::WIN32_FIND_DATAW,
        System::Com::{CoCreateInstance, CoTaskMemFree, IPersistFile, IServiceProvider, CLSCTX_ALL, STGM_READ},
        UI::{
            Shell::{
                IFolderView, IShellBrowser, IShellItemArray, IShellLinkW, IShellWindows, ShellLink, ShellWindows,
                SID_STopLevelBrowser,
                SIGDN_FILESYSPATH, SVGIO_SELECTION, SWC_DESKTOP, SWFO_NEEDDISPATCH,
            },
            WindowsAndMessaging::{GetAncestor, IsWindowVisible, GA_ROOT},
        },
    },
};

/// Where the Space key was pressed.
pub enum Target {
    /// An Explorer window (top-level HWND as an integer so it can cross threads).
    Folder(isize),
    /// The desktop icons.
    Desktop,
}

/// The first selected item's real path, and whether it came from a .lnk shortcut (resolved to its target).
/// The thread must have COM initialised.
pub fn selected_file(target: Target) -> Option<(PathBuf, bool)> {
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        let browser = match target {
            Target::Folder(raw) => folder_browser(&windows, HWND(raw as *mut c_void))?,
            Target::Desktop => desktop_browser(&windows)?,
        };
        let path = first_selected_path(&browser)?;
        let is_lnk = path.extension().map(|e| e.eq_ignore_ascii_case("lnk")).unwrap_or(false);
        if is_lnk {
            if let Some(target) = resolve_shortcut(&path) {
                if target.exists() {
                    return Some((target, true));
                }
            }
        }
        Some((path, false))
    }
}

/// Where a .lnk shortcut really points.
unsafe fn resolve_shortcut(path: &Path) -> Option<PathBuf> {
    let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_ALL).ok()?;
    let file: IPersistFile = link.cast().ok()?;
    file.Load(&HSTRING::from(path.as_os_str()), STGM_READ).ok()?;
    let mut buffer = [0u16; 1024];
    let mut data = WIN32_FIND_DATAW::default();
    link.GetPath(&mut buffer, &mut data, 0).ok()?;
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let target = String::from_utf16_lossy(&buffer[..len]);
    (!target.is_empty()).then(|| PathBuf::from(target))
}

unsafe fn folder_browser(windows: &IShellWindows, explorer_window: HWND) -> Option<IShellBrowser> {
    let count = windows.Count().ok()?;

    // Explorer 11 tabs: several entries share one top-level window; the active tab's window is visible.
    let mut active: Option<IShellBrowser> = None;
    let mut any: Option<IShellBrowser> = None;
    for i in 0..count {
        let Ok(dispatch) = windows.Item(&VARIANT::from(i)) else { continue };
        let Ok(provider) = dispatch.cast::<IServiceProvider>() else { continue };
        let Ok(browser) = provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else { continue };
        let Ok(tab) = browser.GetWindow() else { continue };
        if GetAncestor(tab, GA_ROOT) != explorer_window {
            continue;
        }
        if IsWindowVisible(tab).as_bool() {
            active = Some(browser);
            break;
        }
        any.get_or_insert(browser);
    }
    active.or(any)
}

/// The desktop is not in the normal window list; ask for it explicitly.
/// (If the compiler complains about the constant types here, pass the raw numbers: SWC_DESKTOP = 8, SWFO_NEEDDISPATCH = 1.)
unsafe fn desktop_browser(windows: &IShellWindows) -> Option<IShellBrowser> {
    let mut hwnd = 0i32;
    let dispatch = windows
        .FindWindowSW(&VARIANT::from(0i32), &VARIANT::default(), SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)
        .ok()?;
    let provider: IServiceProvider = dispatch.cast().ok()?;
    provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser).ok()
}

unsafe fn first_selected_path(browser: &IShellBrowser) -> Option<PathBuf> {
    let view = browser.QueryActiveShellView().ok()?;
    let folder_view: IFolderView = view.cast().ok()?;
    let items: IShellItemArray = folder_view.Items(SVGIO_SELECTION).ok()?;
    if items.GetCount().ok()? == 0 {
        return None;
    }
    let name = items.GetItemAt(0).ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let path = name.to_string().ok();
    CoTaskMemFree(Some(name.0 as *const c_void));
    path.map(PathBuf::from)
}
