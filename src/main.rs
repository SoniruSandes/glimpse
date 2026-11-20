// No console window on Windows. The same executable runs in two modes:
//   quicklook                       -> tray app (Space trigger + tray icon)
//   quicklook <path>                -> preview window for one file or folder
//   quicklook --preview <path>      -> same (used internally; flags: --always-pin, --via-shortcut)
#![windows_subsystem = "windows"]

mod archive;
mod content;
mod folder;
mod highlight;
mod htmlview;
mod icons;
mod launcher;
mod markdown;
mod media;
mod model;
mod office;
mod passwords;
mod pdf;
mod preview;
mod settings;
mod theme;
mod tray;
mod window;

#[cfg(windows)]
mod explorer;
#[cfg(windows)]
mod hook;
#[cfg(target_os = "linux")]
mod trigger_linux;

use std::path::PathBuf;

fn main() {
    let mut options = preview::Options { path: PathBuf::new(), always_pin: false, via_shortcut: false };
    let mut have_path = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--always-pin" => options.always_pin = true,
            "--via-shortcut" => options.via_shortcut = true,
            "--tray" => {}
            "--preview" => {
                if let Some(path) = args.next() {
                    options.path = PathBuf::from(path);
                    have_path = true;
                }
            }
            _ => {
                options.path = PathBuf::from(arg);
                have_path = true;
            }
        }
    }

    if have_path {
        preview::run(options);
    } else {
        archive::cleanup_stale();
        tray::run();
    }
}
