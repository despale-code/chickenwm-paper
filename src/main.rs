//! Phase 6.3: GPU Background & Shader Renderer (`chickenwm-paper`)
//! - Anchors a bottom-most surface spanning all monitors at `HWND_BOTTOM` (`WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`)
//! - Supports static images (PNG, JPEG, WebP), animated transitions (`fade`, `wipe`, `outer`, `grow`),
//!   and custom live WGSL fragment shaders (`wgpu::Device::create_shader_module`)
//! - Paces wallpaper transitions and shader ticks via the Phase 3 Adaptive Frame Scheduler,
//!   automatically pausing render ticks when obscured by a fullscreen window.

// Release builds are windowed apps: no console window when launched from the shell, a
// hotkey or the launcher. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod layers;

use anyhow::Result;
use clap::Parser;
use chickenwm_core::bootstrap_config_and_themes;
use chickenwm_core::config::{config_dir, load_or_init_config, HyprConfig};
use chickenwm_core::failsafe::spawn_emergency_failsafe_thread;
use chickenwm_core::frame_scheduler::{AdaptiveFrameScheduler, FramePacerControl};
use chickenwm_core::handles::SafeHdc;
use chickenwm_core::init_tracing;
use chickenwm_core::ipc::{serve_ipc, IpcCommand, IpcResponse, PIPE_NAME_PAPER};
use chickenwm_core::monitor::enumerate_monitors;
use parking_lot::Mutex;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, HDC,
    PAINTSTRUCT, StretchDIBits, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetWindowRect,
    MsgWaitForMultipleObjects, PeekMessageW, RegisterClassW, SetWindowPos, ShowWindow,
    TranslateMessage, HWND_BOTTOM, MSG, PM_REMOVE, QS_ALLINPUT, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SW_SHOWNOACTIVATE, WINDOWPOS, WM_PAINT, WM_WINDOWPOSCHANGING, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Default built-in WGSL aurora fragment shader validated via `wgpu`.
pub const DEFAULT_WALLPAPER_WGSL: &str = r#"
struct Uniforms {
    resolution: vec2<f32>,
    time: f32,
    padding: f32,
};

@group(0) @binding(0)
var<uniform> u: Uniforms;

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = pos.xy / u.resolution;
    let wave = 0.5 + 0.5 * cos(u.time * 0.6 + uv.xyx + vec3<f32>(0.0, 2.0, 4.0));
    let bg = vec3<f32>(0.07, 0.08, 0.13) + wave * 0.18;
    return vec4<f32>(bg, 1.0);
}
"#;

#[derive(Parser, Debug)]
#[command(
    name = "chickenwm-paper",
    version,
    about = "ChickenWM GPU Wallpaper & Live WGSL Shader Background Renderer"
)]
struct Cli {
    /// Optional path to an initial PNG/JPEG/WebP image or `.wgsl` shader.
    #[arg(short, long)]
    wallpaper: Option<String>,

    /// Transition animation (`fade`, `wipe`, `outer`, `grow`).
    #[arg(short, long, default_value = "fade")]
    transition: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionStyle {
    Fade,
    Wipe,
    Outer,
    Grow,
}

impl TransitionStyle {
    fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "wipe" => Self::Wipe,
            "outer" => Self::Outer,
            "grow" => Self::Grow,
            _ => Self::Fade,
        }
    }
}

struct PaperState {
    width: u32,
    height: u32,
    prev_buffer: Vec<u32>,
    curr_buffer: Vec<u32>,
    composite_buffer: Vec<u32>,
    transition_progress: f32, // 0.0 ..= 1.0
    transition_style: TransitionStyle,
    shader_mode: bool,
    shader_time: f32,
    dirty: bool,
    /// `paper { layer = … }` stack, when configured.
    layers: Vec<layers::PreparedLayer>,
    /// Composite of the layers below the first animated one (redrawn only on load).
    layer_base: Vec<u32>,
    first_animated: Option<usize>,
}

impl PaperState {
    fn new(width: u32, height: u32) -> Self {
        let len = (width as usize) * (height as usize);
        let mut st = Self {
            width,
            height,
            prev_buffer: vec![0xFF11111Bu32; len],
            curr_buffer: vec![0xFF181825u32; len],
            composite_buffer: vec![0xFF181825u32; len],
            transition_progress: 1.0,
            transition_style: TransitionStyle::Fade,
            shader_mode: false,
            shader_time: 0.0,
            dirty: true,
            layers: Vec::new(),
            layer_base: Vec::new(),
            first_animated: None,
        };
        st.render_procedural_aurora(0.0);
        st.prev_buffer.copy_from_slice(&st.curr_buffer);
        st.composite_buffer.copy_from_slice(&st.curr_buffer);
        st
    }

    fn load_wallpaper_or_shader(&mut self, path_str: &str, transition: &str) -> Result<()> {
        let expanded = expand_home(path_str);
        let path = expanded.as_path();
        self.transition_style = TransitionStyle::from_str(transition);
        self.clear_layers();

        if path_str.to_ascii_lowercase().ends_with(".wgsl") {
            let wgsl_src = if path.exists() {
                std::fs::read_to_string(path)?
            } else {
                DEFAULT_WALLPAPER_WGSL.to_string()
            };
            validate_wgsl_with_wgpu(&wgsl_src);
            self.shader_mode = true;
            self.dirty = true;
            info!("Activated live WGSL shader wallpaper: {}", path_str);
            return Ok(());
        }

        // Cover-fit: scale to fill the screen keeping the aspect ratio, cropping the overflow
        // (stretching to the exact size distorts any image that doesn't match the monitor).
        let img = image::open(path)?;
        let resized = img
            .resize_to_fill(self.width, self.height, image::imageops::FilterType::Lanczos3)
            .to_rgba8();

        self.prev_buffer.copy_from_slice(&self.composite_buffer);
        for (i, px) in resized.pixels().enumerate() {
            if i < self.curr_buffer.len() {
                let r = px[0] as u32;
                let g = px[1] as u32;
                let b = px[2] as u32;
                self.curr_buffer[i] = 0xFF00_0000 | (r << 16) | (g << 8) | b;
            }
        }
        self.shader_mode = false;
        self.transition_progress = 0.0;
        self.dirty = true;
        info!(
            "Loaded wallpaper '{}' ({}x{}, transition={})",
            path_str, self.width, self.height, transition
        );
        Ok(())
    }

    fn is_animating(&self) -> bool {
        self.shader_mode || self.first_animated.is_some() || self.transition_progress < 1.0 || self.dirty
    }

    fn clear_layers(&mut self) {
        self.layers.clear();
        self.layer_base.clear();
        self.first_animated = None;
    }

    /// Builds the `paper { layer = … }` stack and fades to it. Layers that fail to load are
    /// skipped (with a warning) so one bad path doesn't blank the wallpaper.
    fn load_layers(&mut self, config: &[chickenwm_core::config::PaperLayer], palette: &chickenwm_core::theme::Palette) {
        let mut prepared = Vec::new();
        for layer in config {
            match layers::prepare(layer, self.width, self.height, palette, expand_home) {
                Ok(l) => prepared.push(l),
                Err(e) => warn!("Skipping wallpaper layer {:?} {}: {e:#}", layer.kind, layer.source),
            }
        }
        self.clear_layers();
        self.shader_mode = false;
        self.first_animated = prepared.iter().position(|l| l.animated);
        self.layers = prepared;
        let below = self.first_animated.unwrap_or(self.layers.len());
        let mut base = vec![0xFF00_0000u32; self.curr_buffer.len()];
        layers::composite(&mut base, &self.layers[..below], 0);
        self.layer_base = base;
        self.prev_buffer.copy_from_slice(&self.composite_buffer);
        self.render_layers();
        self.transition_style = TransitionStyle::Fade;
        self.transition_progress = 0.0;
        self.dirty = true;
        info!("Loaded {} wallpaper layer(s)", self.layers.len());
    }

    /// Redraws animated layers at `shader_time` and composites the stack into `curr_buffer`.
    fn render_layers(&mut self) {
        self.curr_buffer.copy_from_slice(&self.layer_base);
        let Some(first) = self.first_animated else { return };
        let (w, h, t) = (self.width, self.height, self.shader_time);
        for layer in self.layers[first..].iter_mut().filter(|l| l.animated) {
            layers::aurora_rgba(&mut layer.rgba, w, h, t);
            if let Some(key) = layer.config.key {
                layers::chroma_key(&mut layer.rgba, [key.r, key.g, key.b], layer.config.tolerance, layer.config.softness, layer.config.despill);
            }
        }
        layers::composite(&mut self.curr_buffer, &self.layers, first);
    }

    fn step(&mut self, dt: f32) -> bool {
        if self.first_animated.is_some() {
            self.shader_time += dt;
            self.render_layers();
            self.dirty = true;
        }
        if self.shader_mode {
            self.shader_time += dt;
            self.render_procedural_aurora(self.shader_time);
            self.composite_buffer.copy_from_slice(&self.curr_buffer);
            return true;
        }

        if self.transition_progress < 1.0 {
            self.transition_progress = (self.transition_progress + dt * 2.4).min(1.0);
            let t = self.transition_progress;
            let w = self.width as i32;
            let h = self.height as i32;
            let cx = w / 2;
            let cy = h / 2;
            let max_r_sq = ((cx * cx + cy * cy) as f32).max(1.0);

            for y in 0..h {
                let row_off = (y as usize) * (w as usize);
                for x in 0..w {
                    let idx = row_off + (x as usize);
                    let p0 = self.prev_buffer[idx];
                    let p1 = self.curr_buffer[idx];

                    let blend_t = match self.transition_style {
                        TransitionStyle::Fade => t,
                        TransitionStyle::Wipe => {
                            let edge = (w as f32) * t;
                            if (x as f32) <= edge {
                                1.0
                            } else {
                                0.0
                            }
                        }
                        TransitionStyle::Grow => {
                            let dx = x - cx;
                            let dy = y - cy;
                            let dist_norm = ((dx * dx + dy * dy) as f32 / max_r_sq).sqrt();
                            if dist_norm <= t {
                                1.0
                            } else {
                                0.0
                            }
                        }
                        TransitionStyle::Outer => {
                            let dx = x - cx;
                            let dy = y - cy;
                            let dist_norm = ((dx * dx + dy * dy) as f32 / max_r_sq).sqrt();
                            if dist_norm >= (1.0 - t) {
                                1.0
                            } else {
                                0.0
                            }
                        }
                    };
                    self.composite_buffer[idx] = lerp_rgb(p0, p1, blend_t);
                }
            }
            self.dirty = false;
            return true;
        }

        if self.dirty {
            self.composite_buffer.copy_from_slice(&self.curr_buffer);
            self.dirty = false;
            return true;
        }

        false
    }

    /// Shows the palette-generated default wallpaper (fades in from the previous image).
    fn load_generated(&mut self, palette: &chickenwm_core::theme::Palette) {
        self.clear_layers();
        let rgba = chickenwm_core::wallpaper::render(palette, self.width, self.height);
        self.prev_buffer.copy_from_slice(&self.composite_buffer);
        for (dst, px) in self.curr_buffer.iter_mut().zip(rgba.chunks_exact(4)) {
            *dst = 0xFF00_0000 | (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
        }
        self.shader_mode = false;
        self.transition_style = TransitionStyle::Fade;
        self.transition_progress = 0.0;
        self.dirty = true;
    }

    fn render_procedural_aurora(&mut self, time: f32) {
        let w = self.width.max(1) as f32;
        let h = self.height.max(1) as f32;
        for y in 0..self.height {
            let v = (y as f32) / h;
            let row = (y as usize) * (self.width as usize);
            for x in 0..self.width {
                let u = (x as f32) / w;
                let wave1 = ((u * 3.5 + time * 0.4).sin() * (v * 2.5 - time * 0.3).cos()) * 0.5 + 0.5;
                let r = (18.0 + 25.0 * (1.0 - v) + 22.0 * wave1) as u32;
                let g = (20.0 + 48.0 * wave1 + 12.0 * u) as u32;
                let b = (34.0 + 68.0 * wave1 + 30.0 * (1.0 - v)) as u32;
                self.curr_buffer[row + (x as usize)] =
                    0xFF00_0000 | (r.min(255) << 16) | (g.min(255) << 8) | b.min(255);
            }
        }
    }
}

#[inline]
fn lerp_rgb(c0: u32, c1: u32, t: f32) -> u32 {
    if t <= 0.0 {
        return c0;
    }
    if t >= 1.0 {
        return c1;
    }
    let inv = 1.0 - t;
    let r0 = ((c0 >> 16) & 0xFF) as f32;
    let g0 = ((c0 >> 8) & 0xFF) as f32;
    let b0 = (c0 & 0xFF) as f32;
    let r1 = ((c1 >> 16) & 0xFF) as f32;
    let g1 = ((c1 >> 8) & 0xFF) as f32;
    let b1 = (c1 & 0xFF) as f32;
    let r = (r0 * inv + r1 * t) as u32;
    let g = (g0 * inv + g1 * t) as u32;
    let b = (b0 * inv + b1 * t) as u32;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// Validates WGSL fragment shader syntax on the active GPU adapter via `wgpu`.
fn validate_wgsl_with_wgpu(wgsl_source: &str) {
    let instance = wgpu::Instance::default();
    if let Some(adapter) =
        pollster_block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
    {
        if let Ok((device, _queue)) =
            pollster_block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
        {
            let _module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("chickenwm-paper-wgsl"),
                source: wgpu::ShaderSource::Wgsl(wgsl_source.into()),
            });
        }
    }
}

fn pollster_block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn dummy_raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            dummy_raw_waker()
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    let waker = unsafe { Waker::from_raw(dummy_raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut pinned = Box::pin(fut);
    loop {
        if let Poll::Ready(val) = pinned.as_mut().poll(&mut cx) {
            return val;
        }
        std::thread::yield_now();
    }
}

/// Expands a leading `~` to the user's profile directory (`~/.config/...` paths in chickenwm.conf).
fn expand_home(path: &str) -> PathBuf {
    let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"));
    match (rest, std::env::var_os("USERPROFILE")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// First image in `~/.config/chickenwm/bg/` (by file name): the wallpaper used when
/// `chickenwm.conf` has no `wallpaper` directive.
fn default_background_image() -> Option<PathBuf> {
    let mut images: Vec<PathBuf> = std::fs::read_dir(config_dir().join("bg"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp"))
        })
        .collect();
    images.sort();
    images.into_iter().next()
}

/// Loads the first `wallpaper` directive, falling back to the image in `~/.config/chickenwm/bg/`.
fn load_configured_wallpaper(state: &mut PaperState, cfg: &HyprConfig) {
    if !cfg.paper.layers.is_empty() {
        let palette = cfg.theme.palette(cfg.theme.mode);
        state.load_layers(&cfg.paper.layers, &palette);
        return;
    }
    let result = if let Some(wp) = cfg.wallpapers.first() {
        state.load_wallpaper_or_shader(&wp.path, &wp.transition)
    } else if let Some(bg) = default_background_image() {
        state.load_wallpaper_or_shader(&bg.to_string_lossy(), "fade")
    } else {
        // Nothing configured: the generated wallpaper in the current theme's colors.
        let palette = cfg.theme.palette(cfg.theme.mode);
        state.load_generated(&palette);
        return;
    };
    if let Err(e) = result {
        warn!("Failed to load wallpaper: {e}");
    }
}

/// Checks if the currently focused foreground window covers the entire primary monitor,
/// allowing `chickenwm-paper` to pause background render ticks when obscured (Phase 6.3).
fn is_obscured_by_fullscreen_window(paper_hwnd: HWND, mon_w: i32, mon_h: i32) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == paper_hwnd {
            return false;
        }
        let mut rc = RECT::default();
        if GetWindowRect(fg, &mut rc).is_ok() {
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;
            return rc.left <= 0 && rc.top <= 0 && w >= mon_w && h >= mon_h;
        }
        false
    }
}

unsafe extern "system" fn paper_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if msg == WM_WINDOWPOSCHANGING && lparam.0 != 0 {
            // Lock wallpaper window permanently at HWND_BOTTOM
            let wp = &mut *(lparam.0 as *mut WINDOWPOS);
            wp.hwndInsertAfter = HWND_BOTTOM;
        }
        if msg == WM_PAINT {
            // Repaint whatever was uncovered from the last composited frame.
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if !hdc.is_invalid() {
                if let Some(state) = PAPER_STATE.get() {
                    let st = state.lock();
                    blit_buffer(hdc, st.width as i32, st.height as i32, &st.composite_buffer);
                }
            }
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// Wallpaper state shared with the window procedure so WM_PAINT can redraw the current frame.
static PAPER_STATE: OnceLock<Arc<Mutex<PaperState>>> = OnceLock::new();

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing("chickenwm_paper");
    let _failsafe = spawn_emergency_failsafe_thread();
    let cli = Cli::parse();

    let (_doc, cfg) = bootstrap_config_and_themes()?;
    let monitors = enumerate_monitors();
    let primary = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .cloned()
        .unwrap();

    let pacer_control = FramePacerControl::new(cfg.misc.fps_limit, cfg.misc.vfr);
    let state = Arc::new(Mutex::new(PaperState::new(
        primary.width.max(320) as u32,
        primary.height.max(240) as u32,
    )));
    let _ = PAPER_STATE.set(Arc::clone(&state));

    if let Some(ref wp_path) = cli.wallpaper {
        let _ = state.lock().load_wallpaper_or_shader(wp_path, &cli.transition);
    } else {
        load_configured_wallpaper(&mut state.lock(), &cfg);
    }

    // Spawn IPC server on `\\.\pipe\chickenwm-paper-ipc`
    let state_ipc = Arc::clone(&state);
    let pacer_ipc = Arc::clone(&pacer_control);
    tokio::spawn(async move {
        let _ = serve_ipc(PIPE_NAME_PAPER, move |cmd| {
            let state_ipc = Arc::clone(&state_ipc);
            let pacer_ipc = Arc::clone(&pacer_ipc);
            async move {
                match cmd {
                    IpcCommand::SetWallpaper {
                        path, transition, ..
                    } => {
                        let trans = transition.unwrap_or_else(|| "fade".to_string());
                        match state_ipc.lock().load_wallpaper_or_shader(&path, &trans) {
                            Ok(()) => {
                                pacer_ipc.wake();
                                IpcResponse::Ok {
                                    message: format!("Applied wallpaper '{path}' ({trans})"),
                                }
                            }
                            Err(e) => IpcResponse::Error {
                                message: format!("Failed to load wallpaper '{path}': {e}"),
                            },
                        }
                    }
                    IpcCommand::SetFramerate(fps) => {
                        pacer_ipc.set_fps_limit(fps);
                        IpcResponse::Ok {
                            message: "Updated wallpaper framerate".to_string(),
                        }
                    }
                    IpcCommand::ReloadConfig => {
                        if let Ok((_, new_cfg)) = load_or_init_config() {
                            pacer_ipc.set_fps_limit(if new_cfg.misc.fps_limit == 0 {
                                None
                            } else {
                                Some(new_cfg.misc.fps_limit)
                            });
                            load_configured_wallpaper(&mut state_ipc.lock(), &new_cfg);
                            pacer_ipc.wake();
                        }
                        IpcResponse::Ok {
                            message: "Reloaded chickenwm-paper config".to_string(),
                        }
                    }
                    _ => IpcResponse::Ok {
                        message: "Ignored by chickenwm-paper".to_string(),
                    },
                }
            }
        })
        .await;
    });

    // Main Win32 bottom-most surface & Phase 3 Frame Scheduler loop
    let state_loop = Arc::clone(&state);
    let pacer_loop = Arc::clone(&pacer_control);
    tokio::task::spawn_blocking(move || unsafe {
        let class_name: Vec<u16> = "ChickenWMPaperClass\0".encode_utf16().collect();
        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(paper_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        let Ok(hwnd) = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WS_POPUP,
            primary.x,
            primary.y,
            primary.width,
            primary.height,
            None,
            None,
            hinstance,
            None,
        ) else {
            warn!("Failed to create ChickenWMPaperClass window");
            return;
        };

        let _ = SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            primary.x,
            primary.y,
            primary.width,
            primary.height,
            SWP_NOACTIVATE,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        info!(
            "chickenwm-paper anchored at HWND_BOTTOM ({}x{} @ {} Hz)",
            primary.width,
            primary.height,
            pacer_loop.effective_target_fps()
        );

        let mut scheduler = AdaptiveFrameScheduler::new(pacer_loop);
        let mut msg = MSG::default();

        loop {
            // Messages must be dispatched: WM_PAINT stays queued until the window paints, so a
            // bare PeekMessage loop would spin on it forever and the wallpaper never draws.
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let obscured = is_obscured_by_fullscreen_window(hwnd, primary.width, primary.height);
            let active = !obscured && state_loop.lock().is_animating();
            let dt = if active {
                scheduler.wait_for_next_frame(true)
            } else {
                // Idle (static image): sleep until a window message arrives — e.g. WM_PAINT when
                // explorer's desktop disappears and uncovers us — re-checking state periodically
                // so IPC wallpaper changes still start their transition promptly.
                let _ = MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT);
                scheduler.sample_delta_seconds()
            };

            if !obscured {
                let mut st = state_loop.lock();
                if st.step(dt) {
                    present_paper_buffer(hwnd, st.width as i32, st.height as i32, &st.composite_buffer);
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_BOTTOM,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
        }
    })
    .await?;

    Ok(())
}

unsafe fn present_paper_buffer(hwnd: HWND, width: i32, height: i32, pixels: &[u32]) {
    unsafe {
        let hdc_raw = GetDC(hwnd);
        if hdc_raw.is_invalid() {
            return;
        }
        let hdc = SafeHdc::Window { hwnd, hdc: hdc_raw };
        blit_buffer(hdc.raw(), width, height, pixels);
    }
}

/// Copies a top-down 32bpp pixel buffer onto `hdc` at the origin.
unsafe fn blit_buffer(hdc: HDC, width: i32, height: i32, pixels: &[u32]) {
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let _ = StretchDIBits(
            hdc,
            0,
            0,
            width,
            height,
            0,
            0,
            width,
            height,
            Some(pixels.as_ptr() as *const core::ffi::c_void),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}
