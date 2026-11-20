# Glimpse (Quick-look) Pre-Release v0.7

Spacebar file preview, like macOS Quick Look, for **Windows** and **Linux**. A tray icon, no dashboard.

| Platform | Built by CI | Trigger |
|---|---|---|
| Windows 10 / 11, x64 | `windows-x64` | Space in Explorer and on the desktop (keyboard hook) |
| Windows 10 / 11, ARM64 | `windows-arm64` | same |
| Linux x86-64 (X11 / Wayland) | `linux-x64` | Space in **GNOME Files** (Nautilus), or `quicklook <path>` |
| Linux ARM64 | `linux-arm64` | same |

Not supported: Windows 7 / 8 (current Rust, WebView2 and the Windows PDF renderer all need Windows 10+),
macOS. 32-bit Windows would build with the `i686-pc-windows-msvc` target but isn't in CI.

## New in 0.7

- **Player:** the played part of the seek and volume bars is coloured; video controls appear over the picture
  while the mouse moves (or when paused) and fade out; the window takes the video's shape (no black bars);
  audio opens a compact 630 x 380 player.
- **Touchpad gestures over the player:** two-finger swipe up / down = volume, left / right = seek (a swipe across
  the seek bar moves half the track). Directions follow your system's touchpad scroll setting.
- **Markdown:** syntax-highlighted code with a copy button, an outline panel with section tracking, Find (Ctrl+F),
  `#anchor` links, front matter shown as a card, text size (Ctrl/Alt +/-, Ctrl+wheel).
- **Icons:** `icons/tray.ico`, `icons/app.ico`, `icons/window.png` (see `icons/README.txt`).

## Build

Push to GitHub: `.github/workflows/build.yml` builds all four targets and uploads artifacts
(tag `v0.6.0` to also create a release). Locally: `cargo build --release`.

Linux build packages: `build-essential pkg-config libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libxkbcommon-dev libwayland-dev libx11-dev`.
If the RAR library gives trouble on a target: `cargo build --release --no-default-features` (everything else still works).

## Run-time pieces that aren't in the executable

| Need | Windows | Linux |
|---|---|---|
| Audio / video | `libmpv-2.dll` next to `quicklook.exe` (a "libmpv" / mpv-dev build from mpv.io) | `libmpv2` (or `libmpv1`) from your package manager |
| PDF | built in (Windows.Data.Pdf) | `libpdfium.so` next to the executable (CI adds it to the Linux packages) |
| Tray icon | built in | GTK 3 + `libayatana-appindicator3` |
| HTML web view | WebView2 (included with Windows 11 and current Windows 10) | not available; code view + "Open in browser" |

Without libmpv / libpdfium everything else works; those file types show a message saying what is missing.

## Using it

- **Windows:** run `quicklook.exe`, select a file or folder in Explorer or on the desktop and press **Space**.
  A `.lnk` shortcut previews its real target and the status bar shows the real path.
- **Linux (GNOME Files):** run `quicklook`; Files calls our previewer when you press Space
  (the same D-Bus interface as GNOME Sushi; if Sushi is installed it owns that name, so remove or stop it).
  Any other file manager: bind `quicklook %f` to a custom action / shortcut. `quicklook <path>` also works on Windows.
- **Tray menu:** Enabled, **Always pin previews**, Start at login, Quit.
- **Always pin previews:** previews stay open when you click away, like Quick Look; **Space / Esc / X** close them,
  and Space on a file in the file manager replaces or closes the preview.

## Controls

| Where | Keys / mouse |
|---|---|
| Any preview | **P** or the pin button: keep this window open as a normal window (min / max / close, resize). Space / Esc close an unpinned preview. |
| Images | **Left / Right** browse the images of the folder (wraps around; sorted "img2" before "img10"); pinch or Ctrl+scroll, **Alt+scroll**, **Alt +/-** zoom; Alt+0 fit; double-click 100%; drag to pan |
| SVG | shown rendered (pure Rust, resvg) and browsable with the other images |
| PDF | wheel scrolls all pages; page pill or PgUp/PgDn/Left/Right/Home/End; password prompt with recent passwords |
| Word / PowerPoint | wheel; page pill (Word pages are approximate) |
| Folders / archives | Up/Down, double-click or Enter to open folders, Backspace = up |
| Audio / video | Play/Pause, seek bar, Mute, volume; click the picture to play/pause. Enter or K play/pause (Space too in a hand-pinned window), Left/Right 5 s, +/- volume, M mute, Home restart |
| HTML | eye = web view (Windows), `</>` = code, arrow = open in browser |

## Notes

- `.webm` / `.mkv` that contain only audio are shown as audio; cover art doesn't count as video.
- Video is software-rendered by mpv (same on every platform) with a small read-ahead cache to keep memory low.
- Remembered PDF passwords: Windows encrypts them with DPAPI; on Linux they're in a mode-0600 file and **not encrypted**
  ("Remember" starts unticked there).
- Icons: Phosphor (MIT), `assets/`.
