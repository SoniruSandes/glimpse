//! Audio and video through libmpv, loaded at run time (libmpv-2.dll on Windows, libmpv.so.2 on Linux).
//! Video is drawn by mpv's *software* renderer into a buffer on a render thread and shown as an egui
//! texture, so there are no native child windows and it behaves the same on Windows and Linux
//! (X11 and Wayland, x86-64 and ARM). The controls are egui widgets.

use std::{
    ffi::{c_char, c_int, c_void, CStr, CString},
    path::{Path, PathBuf},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    thread::JoinHandle,
    time::Duration,
};

use eframe::egui::{self, pos2, vec2, Align2, Color32, CursorIcon, FontId, Key, Rect, RichText, Sense, TextureHandle};
use libloading::Library;

use crate::{icons, theme};

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "webm", "flv", "m4v", "mpg", "mpeg", "ts", "m2ts", "3gp", "ogv",
];
pub const AUDIO_EXTS: &[&str] = &["mp3", "wav", "flac", "ogg", "oga", "opus", "m4a", "aac", "wma", "aiff", "ape", "mka"];

#[cfg(windows)]
const LIB_NAMES: &[&str] = &["libmpv-2.dll", "mpv-2.dll", "libmpv.dll", "mpv-1.dll"];
#[cfg(target_os = "linux")]
const LIB_NAMES: &[&str] = &["libmpv.so.2", "libmpv.so.1", "libmpv.so"];
#[cfg(not(any(windows, target_os = "linux")))]
const LIB_NAMES: &[&str] = &["libmpv.dylib"];

#[cfg(windows)]
const MISSING_LIB: &str = "Audio and video need libmpv. Download a libmpv build for Windows (mpv.io -> Installation -> \
Windows, \"libmpv\" / mpv-dev), extract libmpv-2.dll and put it next to quicklook.exe.";
#[cfg(not(windows))]
const MISSING_LIB: &str = "Audio and video need libmpv. Install it with your package manager \
(Debian/Ubuntu: sudo apt install libmpv2, or libmpv1 on older releases; Fedora: sudo dnf install mpv-libs).";

// ---------------------------------------------------------------------------------------
// libmpv, loaded at run time
// ---------------------------------------------------------------------------------------

type MpvHandle = *mut c_void;

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

struct Api {
    create: unsafe extern "C" fn() -> MpvHandle,
    initialize: unsafe extern "C" fn(MpvHandle) -> c_int,
    set_option_string: unsafe extern "C" fn(MpvHandle, *const c_char, *const c_char) -> c_int,
    set_property_string: unsafe extern "C" fn(MpvHandle, *const c_char, *const c_char) -> c_int,
    command: unsafe extern "C" fn(MpvHandle, *const *const c_char) -> c_int,
    get_property: unsafe extern "C" fn(MpvHandle, *const c_char, c_int, *mut c_void) -> c_int,
    get_property_string: unsafe extern "C" fn(MpvHandle, *const c_char) -> *mut c_char,
    free: unsafe extern "C" fn(*mut c_void),
    terminate_destroy: unsafe extern "C" fn(MpvHandle),
    render_context_create: unsafe extern "C" fn(*mut *mut c_void, MpvHandle, *mut RenderParam) -> c_int,
    render_context_render: unsafe extern "C" fn(*mut c_void, *mut RenderParam) -> c_int,
    render_context_update: unsafe extern "C" fn(*mut c_void) -> u64,
    render_context_set_update_callback:
        unsafe extern "C" fn(*mut c_void, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    render_context_free: unsafe extern "C" fn(*mut c_void),
    _lib: Library, // keep last: must outlive the function pointers above
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(load_api).as_ref().map_err(|e| e.clone())
}

unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
    lib.get::<T>(name).map(|s| *s).map_err(|e| format!("libmpv: {e}"))
}

fn load_api() -> Result<Api, String> {
    let exe_dir: Option<PathBuf> = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    for name in LIB_NAMES {
        let candidates = exe_dir
            .iter()
            .map(|d| d.join(name))
            .chain(std::iter::once(PathBuf::from(name)));
        for candidate in candidates {
            if let Ok(lib) = unsafe { Library::new(&candidate) } {
                return unsafe { bind(lib) };
            }
        }
    }
    Err(MISSING_LIB.to_string())
}

unsafe fn bind(lib: Library) -> Result<Api, String> {
    Ok(Api {
        create: sym(&lib, b"mpv_create\0")?,
        initialize: sym(&lib, b"mpv_initialize\0")?,
        set_option_string: sym(&lib, b"mpv_set_option_string\0")?,
        set_property_string: sym(&lib, b"mpv_set_property_string\0")?,
        command: sym(&lib, b"mpv_command\0")?,
        get_property: sym(&lib, b"mpv_get_property\0")?,
        get_property_string: sym(&lib, b"mpv_get_property_string\0")?,
        free: sym(&lib, b"mpv_free\0")?,
        terminate_destroy: sym(&lib, b"mpv_terminate_destroy\0")?,
        render_context_create: sym(&lib, b"mpv_render_context_create\0")?,
        render_context_render: sym(&lib, b"mpv_render_context_render\0")?,
        render_context_update: sym(&lib, b"mpv_render_context_update\0")?,
        render_context_set_update_callback: sym(&lib, b"mpv_render_context_set_update_callback\0")?,
        render_context_free: sym(&lib, b"mpv_render_context_free\0")?,
        _lib: lib,
    })
}

const FMT_FLAG: c_int = 3;
const FMT_DOUBLE: c_int = 5;
const PARAM_API_TYPE: c_int = 1;
const PARAM_SW_SIZE: c_int = 17;
const PARAM_SW_FORMAT: c_int = 18;
const PARAM_SW_STRIDE: c_int = 19;
const PARAM_SW_POINTER: c_int = 20;
const UPDATE_FRAME: u64 = 1;
const MAX_VIDEO_WIDTH: u32 = 1920;

// ---------------------------------------------------------------------------------------
// Render thread
// ---------------------------------------------------------------------------------------

struct Frame {
    w: usize,
    h: usize,
    rgba: Vec<u8>,
}

struct Shared {
    stop: AtomicBool,
    want_w: AtomicU32,
    want_h: AtomicU32,
    wake: (Mutex<bool>, Condvar),
    frame: Mutex<Option<Frame>>,
    ctx: egui::Context,
}

impl Shared {
    fn wake_up(&self) {
        if let Ok(mut flag) = self.wake.0.lock() {
            *flag = true;
        }
        self.wake.1.notify_all();
    }
}

/// Called by mpv (from any thread) when a new video frame is ready.
unsafe extern "C" fn on_update(data: *mut c_void) {
    let shared = &*(data as *const Shared);
    shared.wake_up();
}

fn render_loop(api: &'static Api, render: usize, shared: Arc<Shared>) {
    let render = render as *mut c_void;
    let mut buffer: Vec<u8> = Vec::new();
    let mut last_size = (0u32, 0u32);
    let format = CString::new("rgb0").unwrap();

    loop {
        {
            let (lock, cv) = &shared.wake;
            let Ok(mut flag) = lock.lock() else { return };
            while !*flag && !shared.stop.load(Ordering::Relaxed) {
                match cv.wait_timeout(flag, Duration::from_millis(100)) {
                    Ok((guard, _)) => flag = guard,
                    Err(_) => return,
                }
            }
            *flag = false;
        }
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }

        let new_frame = unsafe { (api.render_context_update)(render) } & UPDATE_FRAME != 0;
        let (w, h) = (shared.want_w.load(Ordering::Relaxed), shared.want_h.load(Ordering::Relaxed));
        if w < 16 || h < 16 {
            continue;
        }
        // Also redraw when only the size changed (window resized while paused).
        if !new_frame && (w, h) == last_size {
            continue;
        }
        last_size = (w, h);

        let stride = ((w as usize * 4) + 63) / 64 * 64; // 64-byte aligned rows
        buffer.resize(stride * h as usize, 0);
        let mut size: [c_int; 2] = [w as c_int, h as c_int];
        let mut stride_param: usize = stride;
        let mut params = [
            RenderParam { kind: PARAM_SW_SIZE, data: size.as_mut_ptr() as *mut c_void },
            RenderParam { kind: PARAM_SW_FORMAT, data: format.as_ptr() as *mut c_void },
            RenderParam { kind: PARAM_SW_STRIDE, data: &mut stride_param as *mut usize as *mut c_void },
            RenderParam { kind: PARAM_SW_POINTER, data: buffer.as_mut_ptr() as *mut c_void },
            RenderParam { kind: 0, data: ptr::null_mut() },
        ];
        if unsafe { (api.render_context_render)(render, params.as_mut_ptr()) } < 0 {
            continue;
        }

        // rgb0 -> tightly packed RGBA with opaque alpha.
        let (wu, hu) = (w as usize, h as usize);
        let mut rgba = Vec::with_capacity(wu * hu * 4);
        for row in 0..hu {
            let line = &buffer[row * stride..row * stride + wu * 4];
            for px in line.chunks_exact(4) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        if let Ok(mut slot) = shared.frame.lock() {
            *slot = Some(Frame { w: wu, h: hu, rgba });
        }
        shared.ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------------------
// MediaView
// ---------------------------------------------------------------------------------------

/// What size the preview window should have for this media.
#[derive(Clone, Copy)]
pub enum WindowHint {
    /// A compact player window.
    Audio,
    /// Same shape as the picture, so there are no black bars. (display width, height of the video)
    Video { w: f32, h: f32 },
}

/// The part of the seek / volume bar that has been played.
const PLAYED: Color32 = Color32::from_rgb(124, 112, 240);

struct PlayState {
    pos: f64,
    dur: f64,
    paused: bool,
    ended: bool,
    muted: bool,
    volume: f64,
}

#[derive(Default)]
struct Actions {
    toggle_play: bool,
    toggle_mute: bool,
}

pub struct MediaView {
    api: &'static Api,
    mpv: MpvHandle,
    render: *mut c_void,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    texture: Option<TextureHandle>,
    /// None until mpv has opened the file; then whether it has a real video track.
    /// (A .webm or .mkv can be audio only; cover art doesn't count as video.)
    has_video: Option<bool>,
    video_size: Option<(f32, f32)>,
    seek_drag: Option<f64>,
    title: String,
    detail: String,
    pos: f64,
    dur: f64,
    /// When the pointer last moved over the picture (the controls show for a moment after that).
    last_motion: f64,
    /// A short message in the corner of the picture ("Volume 65%").
    osd: Option<(String, f64)>,
}

impl MediaView {
    pub fn new(path: &Path, ctx: &egui::Context) -> Result<Self, String> {
        let api = api()?;
        let mpv = unsafe { (api.create)() };
        if mpv.is_null() {
            return Err("libmpv: mpv_create failed".into());
        }
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            want_w: AtomicU32::new(0),
            want_h: AtomicU32::new(0),
            wake: (Mutex::new(false), Condvar::new()),
            frame: Mutex::new(None),
            ctx: ctx.clone(),
        });
        // From here on, Drop cleans up (also on the error paths below).
        let mut view = Self {
            api,
            mpv,
            render: ptr::null_mut(),
            shared,
            thread: None,
            texture: None,
            has_video: None,
            video_size: None,
            seek_drag: None,
            title: String::new(),
            detail: String::new(),
            pos: 0.0,
            dur: 0.0,
            last_motion: -10.0,
            osd: None,
        };

        for (key, value) in [
            ("config", "no"),
            ("terminal", "no"),
            ("msg-level", "all=no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("osc", "no"),
            ("osd-level", "0"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("sub-auto", "no"),
            ("audio-file-auto", "no"),
            ("audio-display", "no"),
            ("vo", "libmpv"),
            ("keep-open", "yes"),
            ("hwdec", "auto-copy-safe"),
            ("volume", "80"),
            // Keep memory low: a preview doesn't need a big read-ahead cache.
            ("cache", "no"),
            ("demuxer-max-bytes", "16MiB"),
            ("demuxer-max-back-bytes", "4MiB"),
            ("vd-lavc-threads", "3"),
            ("sws-scaler", "fast-bilinear"),
        ] {
            view.set_option(key, value);
        }

        if unsafe { (api.initialize)(mpv) } < 0 {
            return Err("libmpv: initialisation failed".into());
        }

        let api_type = CString::new("sw").unwrap();
        let mut params = [
            RenderParam { kind: PARAM_API_TYPE, data: api_type.as_ptr() as *mut c_void },
            RenderParam { kind: 0, data: ptr::null_mut() },
        ];
        let mut render: *mut c_void = ptr::null_mut();
        let created = unsafe { (api.render_context_create)(&mut render, mpv, params.as_mut_ptr()) };
        if created < 0 || render.is_null() {
            return Err("libmpv: the software video renderer isn't available (libmpv too old?)".into());
        }
        view.render = render;
        unsafe {
            (api.render_context_set_update_callback)(render, Some(on_update), Arc::as_ptr(&view.shared) as *mut c_void);
        }
        let (addr, shared) = (render as usize, view.shared.clone());
        view.thread = Some(std::thread::spawn(move || render_loop(api, addr, shared)));

        view.command(&["loadfile", &path.to_string_lossy()]);
        Ok(view)
    }

    // ---- thin wrappers over the C API ----

    fn set_option(&self, key: &str, value: &str) {
        if let (Ok(k), Ok(v)) = (CString::new(key), CString::new(value)) {
            unsafe { (self.api.set_option_string)(self.mpv, k.as_ptr(), v.as_ptr()) };
        }
    }

    fn set_prop(&self, key: &str, value: &str) {
        if let (Ok(k), Ok(v)) = (CString::new(key), CString::new(value)) {
            unsafe { (self.api.set_property_string)(self.mpv, k.as_ptr(), v.as_ptr()) };
        }
    }

    fn command(&self, args: &[&str]) {
        let owned: Vec<CString> = args.iter().filter_map(|a| CString::new(*a).ok()).collect();
        let mut pointers: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        pointers.push(ptr::null());
        unsafe { (self.api.command)(self.mpv, pointers.as_ptr()) };
    }

    fn get_f64(&self, key: &str) -> Option<f64> {
        let k = CString::new(key).ok()?;
        let mut value = 0f64;
        let rc = unsafe { (self.api.get_property)(self.mpv, k.as_ptr(), FMT_DOUBLE, &mut value as *mut f64 as *mut c_void) };
        (rc >= 0).then_some(value)
    }

    fn get_flag(&self, key: &str) -> Option<bool> {
        let k = CString::new(key).ok()?;
        let mut value: c_int = 0;
        let rc = unsafe { (self.api.get_property)(self.mpv, k.as_ptr(), FMT_FLAG, &mut value as *mut c_int as *mut c_void) };
        (rc >= 0).then_some(value != 0)
    }

    fn get_string(&self, key: &str) -> Option<String> {
        let k = CString::new(key).ok()?;
        unsafe {
            let raw = (self.api.get_property_string)(self.mpv, k.as_ptr());
            if raw.is_null() {
                return None;
            }
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            (self.api.free)(raw as *mut c_void);
            Some(text)
        }
    }

    pub fn status(&self) -> String {
        let kind = match self.has_video {
            Some(true) => "Video",
            Some(false) => "Audio",
            None => "Media",
        };
        format!("{kind}  {} / {}", fmt_time(self.pos), fmt_time(self.dur))
    }

    /// The window shape this media wants (audio: a small player; video: the picture's shape).
    pub fn window_hint(&self) -> Option<WindowHint> {
        match self.has_video {
            Some(false) => Some(WindowHint::Audio),
            Some(true) => self.video_size.map(|(w, h)| WindowHint::Video { w, h }),
            None => None,
        }
    }

    fn show_osd(&mut self, text: String, now: f64) {
        self.osd = Some((text, now));
    }

    fn set_volume(&mut self, volume: f64, now: f64) {
        let v = volume.clamp(0.0, 100.0);
        self.set_prop("volume", &format!("{v:.0}"));
        self.show_osd(format!("Volume {v:.0}%"), now);
    }

    fn seek_by(&mut self, seconds: f64, now: f64) {
        self.command(&["seek", &format!("{seconds:.3}"), "relative+keyframes"]);
        let target = (self.pos + seconds).clamp(0.0, self.dur.max(0.0));
        self.show_osd(format!("{} / {}", fmt_time(target), fmt_time(self.dur)), now);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        ctx.request_repaint_after(Duration::from_millis(100));

        self.pos = self.get_f64("time-pos").unwrap_or(self.pos);
        self.dur = self.get_f64("duration").unwrap_or(self.dur);
        let state = PlayState {
            pos: self.pos,
            dur: self.dur,
            paused: self.get_flag("pause").unwrap_or(false),
            ended: self.get_flag("eof-reached").unwrap_or(false),
            muted: self.get_flag("mute").unwrap_or(false),
            volume: self.get_f64("volume").unwrap_or(80.0),
        };

        if self.has_video.is_none() && self.dur > 0.0 {
            // The file is open: is there a video track (and not just cover art)?
            let has_track = self.get_string("current-tracks/video/id").is_some();
            let cover = self.get_string("current-tracks/video/albumart").map(|v| v == "yes").unwrap_or(false);
            self.has_video = Some(has_track && !cover);
        }
        if self.has_video == Some(true) && self.video_size.is_none() {
            let size = |w: Option<f64>, h: Option<f64>| match (w, h) {
                (Some(w), Some(h)) if w > 1.0 && h > 1.0 => Some((w as f32, h as f32)),
                _ => None,
            };
            self.video_size = size(self.get_f64("video-params/dw"), self.get_f64("video-params/dh"))
                .or_else(|| size(self.get_f64("video-params/w"), self.get_f64("video-params/h")));
        }
        if self.title.is_empty() {
            if let Some(t) = self.get_string("media-title") {
                self.title = t;
            }
            let artist = self.get_string("metadata/by-key/artist").unwrap_or_default();
            let album = self.get_string("metadata/by-key/album").unwrap_or_default();
            self.detail = [artist, album].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ·  ");
        }

        // Keyboard: Enter/K play-pause (Space too in a hand-pinned window), Left/Right seek 5 s,
        // + / - volume, M mute, Home restart.
        let space_plays = crate::window::space_plays_media();
        let (toggle, left, right, louder, quieter, mute_key, home) = ctx.input(|i| {
            (
                i.key_pressed(Key::Enter) || i.key_pressed(Key::K) || (space_plays && i.key_pressed(Key::Space)),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals),
                i.key_pressed(Key::Minus),
                i.key_pressed(Key::M),
                i.key_pressed(Key::Home),
            )
        });
        let mut acts = Actions { toggle_play: toggle, toggle_mute: mute_key };
        if left {
            self.seek_by(-5.0, now);
        }
        if right {
            self.seek_by(5.0, now);
        }
        if home {
            self.command(&["seek", "0", "absolute"]);
        }
        if louder || quieter {
            self.set_volume(state.volume + if louder { 5.0 } else { -5.0 }, now);
        }

        // Newest video frame -> texture.
        let frame = self.shared.frame.lock().ok().and_then(|mut slot| slot.take());
        if let Some(frame) = frame {
            let image = egui::ColorImage::from_rgba_unmultiplied([frame.w, frame.h], &frame.rgba);
            match self.texture.as_mut() {
                Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR)),
            }
        }

        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, Sense::hover());

        // Touchpad gestures over the player: two-finger swipe up/down = volume,
        // left/right = seek at half the speed of the finger (a swipe across the seek bar moves half the track).
        let over = ctx.input(|i| i.pointer.hover_pos()).map_or(false, |p| area.contains(p));
        if over {
            let (scroll, modifier) = ctx.input(|i| (i.raw_scroll_delta, i.modifiers.ctrl || i.modifiers.alt));
            if !modifier && (scroll.x != 0.0 || scroll.y != 0.0) {
                if scroll.y.abs() >= scroll.x.abs() {
                    self.set_volume(state.volume + scroll.y as f64 * 0.12, now);
                } else if state.dur > 0.0 {
                    let bar_width = (area.width() - 130.0).max(100.0) as f64;
                    self.seek_by(scroll.x as f64 / bar_width * 0.5 * state.dur, now);
                }
            }
        }

        match self.has_video {
            Some(true) => self.video_surface(ui, &ctx, area, &state, now, &mut acts),
            Some(false) => self.audio_panel(ui, area, &state, &mut acts),
            None => {
                ui.painter().text(
                    area.center(),
                    Align2::CENTER_CENTER,
                    "Opening…",
                    FontId::proportional(16.0),
                    Color32::GRAY,
                );
            }
        }

        // The little message ("Volume 65%").
        if let Some((text, started)) = self.osd.clone() {
            let age = now - started;
            if age < 1.4 {
                let alpha = ((1.4 - age) / 0.4).clamp(0.0, 1.0) as f32;
                let galley = ui.painter().layout_no_wrap(
                    text,
                    FontId::proportional(18.0),
                    Color32::from_white_alpha((255.0 * alpha) as u8),
                );
                let size = galley.size() + vec2(28.0, 14.0);
                let rect = Rect::from_center_size(pos2(area.center().x, area.top() + 44.0), size);
                ui.painter().rect_filled(rect, 14.0, Color32::from_black_alpha((190.0 * alpha) as u8));
                ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
                ctx.request_repaint();
            } else {
                self.osd = None;
            }
        }

        if acts.toggle_play {
            if state.ended {
                self.command(&["seek", "0", "absolute"]);
                self.set_prop("pause", "no");
            } else {
                self.set_prop("pause", if state.paused { "no" } else { "yes" });
            }
        }
        if acts.toggle_mute {
            self.set_prop("mute", if state.muted { "no" } else { "yes" });
        }
    }

    /// The picture fills the whole area; the controls fade in over its bottom edge while the pointer is
    /// moving (or the video is paused) and fade out again.
    fn video_surface(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, area: Rect, state: &PlayState, now: f64, acts: &mut Actions) {
        // Tell the render thread what size to draw at (physical pixels, capped for speed).
        let ppp = ctx.pixels_per_point();
        let mut w = (area.width() * ppp).round().max(16.0) as u32;
        let mut h = (area.height() * ppp).round().max(16.0) as u32;
        if w > MAX_VIDEO_WIDTH {
            h = (h as f32 * MAX_VIDEO_WIDTH as f32 / w as f32).round().max(16.0) as u32;
            w = MAX_VIDEO_WIDTH;
        }
        if self.shared.want_w.swap(w, Ordering::Relaxed) != w || self.shared.want_h.swap(h, Ordering::Relaxed) != h {
            self.shared.wake_up();
        }
        ui.painter().rect_filled(area, 6.0, Color32::BLACK);
        if let Some(texture) = &self.texture {
            ui.painter().image(texture.id(), area, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }

        let hover = ctx.input(|i| i.pointer.hover_pos()).filter(|p| area.contains(*p));
        if hover.is_some() && ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_pressed()) {
            self.last_motion = now;
        }
        let controls_height = 100.0;
        let controls = Rect::from_min_max(pos2(area.min.x, area.max.y - controls_height), area.max);
        let over_controls = hover.map_or(false, |p| controls.contains(p));
        let show = state.paused || state.ended || self.seek_drag.is_some() || over_controls || now - self.last_motion < 2.5;
        let alpha = ctx.animate_bool_with_time(ui.id().with("controls_fade"), show, 0.2);

        // Click the picture to play / pause (the controls, drawn after this, take their own clicks).
        let surface = ui.interact(area, ui.id().with("video_surface"), Sense::click());
        if surface.clicked() {
            acts.toggle_play = true;
        }

        if alpha > 0.01 {
            // A soft dark gradient behind the controls.
            let mut mesh = egui::epaint::Mesh::default();
            let top = Color32::from_black_alpha(0);
            let bottom = Color32::from_black_alpha((205.0 * alpha) as u8);
            mesh.colored_vertex(controls.left_top(), top);
            mesh.colored_vertex(controls.right_top(), top);
            mesh.colored_vertex(controls.right_bottom(), bottom);
            mesh.colored_vertex(controls.left_bottom(), bottom);
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(0, 2, 3);
            ui.painter().add(egui::Shape::mesh(mesh));
            self.controls(ui, controls, state, acts, alpha);
        } else if hover.is_some() && !state.paused {
            ctx.set_cursor_icon(CursorIcon::None);
        }
    }

    /// A compact player: a cover tile and the title on top, the controls underneath.
    fn audio_panel(&mut self, ui: &mut egui::Ui, area: Rect, state: &PlayState, acts: &mut Actions) {
        let controls_height = 92.0;
        let top = Rect::from_min_max(area.min, pos2(area.max.x, area.max.y - controls_height));
        let tile_side = (top.height() - 36.0).clamp(80.0, 150.0);
        let tile = Rect::from_min_size(top.min + vec2(22.0, 18.0), vec2(tile_side, tile_side));
        ui.painter().rect_filled(tile, 14.0, Color32::from_rgb(46, 46, 58));
        ui.painter().text(
            tile.center(),
            Align2::CENTER_CENTER,
            icons::MUSIC_NOTE,
            FontId::new(tile_side * 0.46, theme::icon_family()),
            Color32::from_rgb(168, 158, 255),
        );

        let text_rect = Rect::from_min_max(pos2(tile.right() + 22.0, tile.top()), pos2(top.right() - 22.0, tile.bottom()));
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(text_rect), |ui| {
            ui.vertical(|ui| {
                let title = if self.title.is_empty() { "Audio" } else { self.title.as_str() };
                ui.add(
                    egui::Label::new(RichText::new(title).size(22.0).strong().color(Color32::WHITE))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
                if !self.detail.is_empty() {
                    ui.label(RichText::new(self.detail.as_str()).size(14.5).color(Color32::GRAY));
                }
                ui.label(RichText::new(fmt_time(state.dur)).monospace().color(Color32::GRAY));
            });
        });

        let controls = Rect::from_min_max(pos2(area.min.x, area.max.y - controls_height), area.max);
        self.controls(ui, controls, state, acts, 1.0);
    }

    /// Seek bar with times, then play / mute / volume centred underneath.
    fn controls(&mut self, ui: &mut egui::Ui, rect: Rect, state: &PlayState, acts: &mut Actions, alpha: f32) {
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(18.0, 8.0))), |ui| {
            ui.set_opacity(alpha);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let shown = self.seek_drag.unwrap_or(state.pos);
                    ui.label(RichText::new(fmt_time(shown)).monospace().color(Color32::from_gray(235)));
                    let bar_width = (ui.available_width() - 66.0).max(60.0);
                    let fraction = if state.dur > 0.0 { (shown / state.dur) as f32 } else { 0.0 };
                    let (response, new) = progress_bar(ui, bar_width, fraction);
                    if let Some(f) = new {
                        let target = f as f64 * state.dur;
                        if response.dragged() {
                            self.seek_drag = Some(target);
                            self.command(&["seek", &format!("{target:.3}"), "absolute+keyframes"]);
                        }
                        if response.drag_stopped() || (response.clicked() && !response.dragged()) {
                            self.command(&["seek", &format!("{target:.3}"), "absolute+exact"]);
                            self.seek_drag = None;
                        }
                    }
                    if response.drag_stopped() {
                        self.seek_drag = None;
                    }
                    ui.label(RichText::new(fmt_time(state.dur)).monospace().color(Color32::from_gray(235)));
                });

                ui.horizontal(|ui| {
                    ui.add_space(((ui.available_width() - 300.0) / 2.0).max(0.0));
                    let play_icon = if state.ended {
                        icons::ARROW_COUNTER_CLOCKWISE
                    } else if state.paused {
                        icons::PLAY
                    } else {
                        icons::PAUSE
                    };
                    if theme::icon_button(ui, play_icon, "Play / pause (Enter, K)", false, true).clicked() {
                        acts.toggle_play = true;
                    }
                    let speaker = if state.muted || state.volume <= 0.5 {
                        icons::SPEAKER_X
                    } else if state.volume < 40.0 {
                        icons::SPEAKER_LOW
                    } else {
                        icons::SPEAKER_HIGH
                    };
                    if theme::icon_button(ui, speaker, "Mute (M)", state.muted, true).clicked() {
                        acts.toggle_mute = true;
                    }
                    let (response, new) = progress_bar(ui, 130.0, (state.volume / 100.0) as f32);
                    if let (Some(f), true) = (new, response.dragged() || response.clicked()) {
                        let now = ui.ctx().input(|i| i.time);
                        self.set_volume(f as f64 * 100.0, now);
                    }
                    ui.label(RichText::new(format!("{:.0}%", state.volume)).color(Color32::from_gray(225)));
                });
            });
        });
    }
}

/// A thin bar whose played part is coloured, with a round handle. Returns the response and, while the
/// pointer is pressing / dragging, the position under it as 0..=1.
fn progress_bar(ui: &mut egui::Ui, width: f32, fraction: f32) -> (egui::Response, Option<f32>) {
    let (rect, response) = ui.allocate_exact_size(vec2(width, 26.0), Sense::click_and_drag());
    let active = response.hovered() || response.dragged();
    let track_height = if active { 6.0 } else { 4.0 };
    let track = Rect::from_center_size(rect.center(), vec2((rect.width() - 12.0).max(1.0), track_height));
    let fraction = fraction.clamp(0.0, 1.0);
    let x = track.min.x + track.width() * fraction;

    let painter = ui.painter();
    painter.rect_filled(track, 3.0, Color32::from_white_alpha(60));
    painter.rect_filled(Rect::from_min_max(track.min, pos2(x, track.max.y)), 3.0, PLAYED);
    painter.circle_filled(pos2(x, track.center().y), if active { 7.5 } else { 5.5 }, Color32::WHITE);

    let new = response
        .interact_pointer_pos()
        .map(|p| ((p.x - track.min.x) / track.width()).clamp(0.0, 1.0));
    (response, new)
}

impl Drop for MediaView {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake_up();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        unsafe {
            if !self.render.is_null() {
                (self.api.render_context_set_update_callback)(self.render, None, ptr::null_mut());
                (self.api.render_context_free)(self.render);
            }
            (self.api.terminate_destroy)(self.mpv);
        }
    }
}

fn fmt_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
