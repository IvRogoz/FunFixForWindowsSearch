#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app_state;
mod commands;
mod gpu_ui;
mod indexing;
mod indexing_ntfs;
mod platform;
mod search;
mod search_worker;
mod storage;
mod tui_view;

use std::env;
use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use app_state::AppState;
use eframe::egui;
use egui_ratatui::RataguiBackend;
use ratatui::style::Color;
use ratatui::Terminal;
use soft_ratatui::embedded_graphics_unicodefonts::{
    mono_8x13_atlas, mono_8x13_bold_atlas, mono_8x13_italic_atlas,
};
use soft_ratatui::{EmbeddedGraphics, SoftBackend};

#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

const VISIBLE_RESULTS_LIMIT: usize = 600;
const QUERY_DEBOUNCE_DELAY: Duration = Duration::from_millis(70);
const SEARCH_BATCH_SIZE: usize = 12_000;
const FILENAME_INDEX_BUILD_BATCH: usize = 1_000;
const DEFAULT_LATEST_WINDOW_SECS: i64 = 5 * 60;
const DELTA_REFRESH_COOLDOWN: Duration = Duration::from_millis(300);
const FILE_PATH_MAX_CHARS: usize = 86;
const MAX_INDEX_EVENTS_PER_TICK: usize = 2;
const MAX_SEARCH_EVENTS_PER_TICK: usize = 24;
const POLL_INTERVAL_ACTIVE: Duration = Duration::from_millis(16);
const POLL_INTERVAL_IDLE: Duration = Duration::from_millis(55);
const POLL_INTERVAL_HIDDEN: Duration = Duration::from_millis(80);
const UNKNOWN_TS: i64 = i64::MIN;
const KEYBOARD_PAGE_JUMP: usize = 12;
const WINDOW_WIDTH: f32 = 980.0;
const WINDOW_HEIGHT: f32 = 560.0;
const PANEL_ANIMATION_DURATION: Duration = Duration::from_millis(180);
const PANEL_SHOWN_Y: f32 = 0.0;
const PANEL_HIDDEN_Y_EXTRA: f32 = 24.0;

static DEBUG_LOG_FILES: OnceLock<std::sync::Mutex<Vec<std::fs::File>>> = OnceLock::new();
static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();

fn main() -> eframe::Result {
    let _ = DEBUG_ENABLED.set(env::var("WIZMINI_DEBUG").ok().as_deref() == Some("1"));
    let _ = init_debug_log_file();
    std::panic::set_hook(Box::new(|info| {
        debug_log(&format!("panic: {}", info));
    }));

    let window_width = default_window_width();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustSearch")
            .with_inner_size([window_width, WINDOW_HEIGHT])
            .with_decorations(false),
        ..Default::default()
    };

    let start_visible = should_start_visible_from_args();
    let startup_scope = startup_scope_override_from_args();

    eframe::run_native(
        "RustSearch",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(RustSearchEguiApp::new(
                start_visible,
                startup_scope.clone(),
                window_width,
            )))
        }),
    )
}

struct RustSearchEguiApp {
    runtime: AppState,
    renderer: Renderer,
    panel_progress: f32,
    panel_anim_last_tick: Option<Instant>,
    window_width: f32,
    window_height: f32,
    fullscreen_enabled: bool,
    fullheight_enabled: bool,
    fullheight_before_fullscreen: bool,
    last_frame_instant: Instant,
    frame_time_ema_ms: f32,
}

impl RustSearchEguiApp {
    fn new(start_visible: bool, startup_scope: Option<SearchScope>, window_width: f32) -> Self {
        let renderer = Renderer::from_env();

        Self {
            runtime: AppState::new(start_visible, startup_scope),
            renderer,
            panel_progress: if start_visible { 1.0 } else { 0.0 },
            panel_anim_last_tick: None,
            window_width,
            window_height: WINDOW_HEIGHT,
            fullscreen_enabled: false,
            fullheight_enabled: false,
            fullheight_before_fullscreen: false,
            last_frame_instant: Instant::now(),
            frame_time_ema_ms: 0.0,
        }
    }

    fn apply_window_mode_request(&mut self, ctx: &egui::Context, request: WindowModeRequest) {
        match request {
            WindowModeRequest::ToggleFullscreen => {
                if !self.fullscreen_enabled {
                    self.fullheight_before_fullscreen = self.fullheight_enabled;
                    self.fullscreen_enabled = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                } else {
                    self.fullscreen_enabled = false;
                    self.fullheight_enabled = self.fullheight_before_fullscreen;
                    self.window_height = if self.fullheight_enabled {
                        screen_height()
                    } else {
                        WINDOW_HEIGHT
                    };
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                        self.window_width,
                        self.window_height,
                    )));
                }
            }
            WindowModeRequest::ToggleFullHeight => {
                if self.fullscreen_enabled {
                    self.fullscreen_enabled = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                }

                self.fullheight_enabled = !self.fullheight_enabled;
                self.window_height = if self.fullheight_enabled {
                    screen_height()
                } else {
                    WINDOW_HEIGHT
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    self.window_width,
                    self.window_height,
                )));
            }
        }
    }

    fn sync_window_slide(&mut self, ctx: &egui::Context) {
        let target = if self.runtime.panel_visible { 1.0 } else { 0.0 };

        let now = Instant::now();
        let dt = self
            .panel_anim_last_tick
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(Duration::from_millis(16));
        self.panel_anim_last_tick = Some(now);

        let step = (dt.as_secs_f32() / PANEL_ANIMATION_DURATION.as_secs_f32()).clamp(0.01, 0.25);

        if self.panel_progress < target {
            self.panel_progress = (self.panel_progress + step).min(1.0);
        } else if self.panel_progress > target {
            self.panel_progress = (self.panel_progress - step).max(0.0);
        }

        let done = (self.panel_progress - target).abs() <= f32::EPSILON;
        if done {
            self.panel_anim_last_tick = None;
        }

        let shown_y = PANEL_SHOWN_Y;
        let hidden_y = -self.window_height - PANEL_HIDDEN_Y_EXTRA;
        let y = hidden_y + (shown_y - hidden_y) * self.panel_progress;

        let x = centered_window_x(self.window_width);

        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::Pos2::new(x, y)));
    }

    fn apply_hotkeys(&mut self, ctx: &egui::Context) {
        if !self.runtime.panel_visible {
            return;
        }

        let mut enter_pressed = false;
        let mut alt_enter = false;
        let mut close_help_once = false;
        let mut close_help_forever = false;
        let mut any_key_pressed = false;

        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key { pressed, .. } = event {
                    if *pressed {
                        any_key_pressed = true;
                    }
                }
            }

            if i.key_pressed(egui::Key::Escape) {
                self.runtime.on_escape();
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                self.runtime.on_move_down();
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                self.runtime.on_move_up();
            }
            if i.key_pressed(egui::Key::PageDown) {
                self.runtime.on_page_down();
            }
            if i.key_pressed(egui::Key::PageUp) {
                self.runtime.on_page_up();
            }
            if i.key_pressed(egui::Key::Home) {
                self.runtime.on_home();
            }
            if i.key_pressed(egui::Key::End) {
                self.runtime.on_end();
            }

            if i.key_pressed(egui::Key::Enter) {
                enter_pressed = true;
                alt_enter = i.modifiers.alt;
            }

            if self.runtime.show_quick_help_overlay && i.key_pressed(egui::Key::Tab) {
                self.runtime.quick_help_selected_action =
                    (self.runtime.quick_help_selected_action + 1) % 2;
            }

            if self.runtime.show_quick_help_overlay && i.key_pressed(egui::Key::D) {
                close_help_forever = true;
            }

            if self.runtime.show_quick_help_overlay && i.key_pressed(egui::Key::Escape) {
                close_help_once = true;
            }
        });

        if any_key_pressed {
            self.runtime.show_privilege_overlay = false;
            self.runtime.show_quick_help_overlay = false;
            self.runtime.show_about_overlay = false;
        }

        if close_help_once {
            self.runtime.show_quick_help_overlay = false;
        }
        if close_help_forever {
            self.runtime.show_quick_help_overlay = false;
            storage::persist_quick_help_dismissed(true);
        }

        if enter_pressed {
            if alt_enter {
                self.runtime.on_alt_enter();
            } else {
                self.runtime.activate_selected();
            }
        }
    }

    fn apply_query_text_input(&mut self, ctx: &egui::Context) {
        if !self.runtime.panel_visible {
            return;
        }

        let mut raw = self.runtime.raw_query.clone();
        let mut changed = false;

        ctx.input(|i| {
            for event in &i.events {
                match event {
                    egui::Event::Text(text) => {
                        if !text.is_empty() {
                            raw.push_str(text);
                            changed = true;
                        }
                    }
                    egui::Event::Paste(text) => {
                        if !text.is_empty() {
                            raw.push_str(text);
                            changed = true;
                        }
                    }
                    egui::Event::Key {
                        key,
                        pressed,
                        modifiers,
                        ..
                    } => {
                        if !pressed {
                            continue;
                        }

                        if modifiers.ctrl || modifiers.command || modifiers.alt {
                            continue;
                        }

                        match key {
                            egui::Key::Backspace => {
                                if raw.pop().is_some() {
                                    changed = true;
                                }
                            }
                            egui::Key::Delete => {
                                if !raw.is_empty() {
                                    raw.clear();
                                    changed = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        });

        if changed && raw != self.runtime.raw_query {
            self.runtime.on_query_changed(raw);
        }
    }
}

impl eframe::App for RustSearchEguiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let dt_ms = now
            .saturating_duration_since(self.last_frame_instant)
            .as_secs_f32()
            * 1000.0;
        self.last_frame_instant = now;
        if self.frame_time_ema_ms <= f32::EPSILON {
            self.frame_time_ema_ms = dt_ms;
        } else {
            self.frame_time_ema_ms = self.frame_time_ema_ms * 0.9 + dt_ms * 0.1;
        }

        let repaint_after = if self.panel_anim_last_tick.is_some()
            || self.runtime.indexing_in_progress
            || self.runtime.active_search_query.is_some()
        {
            POLL_INTERVAL_ACTIVE
        } else if self.runtime.panel_visible {
            POLL_INTERVAL_IDLE
        } else {
            POLL_INTERVAL_HIDDEN
        };
        ctx.request_repaint_after(repaint_after);

        let tick = self.runtime.process_tick();
        let _ = tick.focus_search;

        if tick.visibility_changed {
            if self.runtime.panel_visible {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        if let Some(request) = tick.window_mode_request {
            self.apply_window_mode_request(ctx, request);
        }
        if let Some(request) = tick.renderer_mode_request {
            self.renderer = Renderer::from_mode(request);
        }
        self.sync_window_slide(ctx);
        if tick.should_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        self.apply_hotkeys(ctx);
        if self.runtime.should_exit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.apply_query_text_input(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .inner_margin(egui::Margin::same(0))
                    .outer_margin(egui::Margin::same(0)),
            )
            .show(ctx, |ui| {
                let hud = RenderHud {
                    frame_time_ms: self.frame_time_ema_ms,
                    repaint_after,
                };
                self.renderer.draw(ctx, ui, &self.runtime, hud);
            });
    }
}

#[derive(Clone, Copy)]
struct RenderHud {
    frame_time_ms: f32,
    repaint_after: Duration,
}

enum Renderer {
    SoftTui(Terminal<RataguiBackend<EmbeddedGraphics>>),
    GpuEgui,
}

impl Renderer {
    fn from_env() -> Self {
        let mode = env::var("RUSTSEARCH_RENDERER")
            .unwrap_or_else(|_| "gpu".to_string())
            .to_ascii_lowercase();

        if mode == "soft" || mode == "ratatui" {
            Self::from_mode(RendererModeRequest::Soft)
        } else {
            Self::from_mode(RendererModeRequest::Gpu)
        }
    }

    fn from_mode(mode: RendererModeRequest) -> Self {
        match mode {
            RendererModeRequest::Gpu => Self::GpuEgui,
            RendererModeRequest::Soft => {
                let font_regular = mono_8x13_atlas();
                let font_italic = mono_8x13_italic_atlas();
                let font_bold = mono_8x13_bold_atlas();
                let soft_backend = SoftBackend::<EmbeddedGraphics>::new(
                    160,
                    60,
                    font_regular,
                    Some(font_bold),
                    Some(font_italic),
                );
                let backend = RataguiBackend::new("rustsearch", soft_backend);
                let terminal = Terminal::new(backend).expect("terminal init failed");
                Self::SoftTui(terminal)
            }
        }
    }

    fn draw(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, app: &AppState, hud: RenderHud) {
        match self {
            Self::SoftTui(terminal) => {
                if let Err(err) = terminal.draw(|frame| {
                    tui_view::draw(frame, app);
                }) {
                    debug_log(&format!("Soft renderer draw failed: {}", err));
                }
                ui.add(terminal.backend_mut());
            }
            Self::GpuEgui => gpu_ui::draw(ctx, ui, app, hud.frame_time_ms, hud.repaint_after),
        }
    }
}

fn should_start_visible_from_args() -> bool {
    !env::args().any(|arg| arg == "--hide" || arg == "--hidden")
}

fn default_window_width() -> f32 {
    #[cfg(target_os = "windows")]
    {
        let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        if screen_w > 0 {
            return ((screen_w as f32) / 3.0).max(WINDOW_WIDTH);
        }
    }

    WINDOW_WIDTH
}

fn screen_height() -> f32 {
    #[cfg(target_os = "windows")]
    {
        let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        if screen_h > 0 {
            return screen_h as f32;
        }
    }

    WINDOW_HEIGHT
}

fn centered_window_x(window_width: f32) -> f32 {
    #[cfg(target_os = "windows")]
    {
        let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        if screen_w > 0 {
            return ((screen_w as f32) - window_width).max(0.0) / 2.0;
        }
    }

    220.0
}

fn startup_scope_override_from_args() -> Option<SearchScope> {
    for arg in env::args() {
        let Some(value) = arg.strip_prefix("--scope=") else {
            continue;
        };

        let lower = value.trim().to_ascii_lowercase();
        if lower == "current-folder" {
            return Some(SearchScope::CurrentFolder);
        }
        if lower == "entire-current-drive" {
            return Some(SearchScope::EntireCurrentDrive);
        }
        if lower == "all-local-drives" {
            return Some(SearchScope::AllLocalDrives);
        }

        let bytes = lower.as_bytes();
        if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return Some(SearchScope::Drive((bytes[0] as char).to_ascii_uppercase()));
        }
    }

    None
}

fn debug_log_path_localappdata() -> std::path::PathBuf {
    let base = env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base)
        .join("WizMini")
        .join("rustsearch-debug.log")
}

fn debug_log_path_exe_dir() -> std::path::PathBuf {
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|v| v.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    exe_dir.join("rustsearch-debug.log")
}

fn init_debug_log_file() -> Result<(), String> {
    let mut files = Vec::new();
    let mut opened_paths = Vec::new();

    for path in [debug_log_path_localappdata(), debug_log_path_exe_dir()] {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            files.push(file);
            opened_paths.push(path.display().to_string());
        }
    }

    if files.is_empty() {
        return Err("failed to open any debug log file".to_string());
    }

    let _ = DEBUG_LOG_FILES.set(std::sync::Mutex::new(files));
    debug_log(&format!(
        "log files initialized at {}",
        opened_paths.join(" | ")
    ));
    Ok(())
}

pub(crate) fn debug_log(message: &str) {
    if !*DEBUG_ENABLED.get_or_init(|| false) {
        return;
    }

    let line = format!("[rustsearch-debug] {}\n", message);

    if let Some(files_mutex) = DEBUG_LOG_FILES.get() {
        if let Ok(mut files) = files_mutex.lock() {
            for file in files.iter_mut() {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
    }

    eprintln!("{}", line.trim_end());
}

#[derive(Debug, Clone)]
pub(crate) struct SearchItem {
    pub(crate) path: Box<str>,
    pub(crate) modified_unix_secs: i64,
}

pub(crate) enum IndexEvent {
    SnapshotLoaded {
        job_id: u64,
        items: Vec<SearchItem>,
    },
    Progress {
        job_id: u64,
        current: usize,
        total: usize,
        phase: &'static str,
    },
    Done {
        job_id: u64,
        items: Vec<SearchItem>,
        backend: IndexBackend,
    },
    Delta {
        job_id: u64,
        upserts: Vec<SearchItem>,
        deleted_paths: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowModeRequest {
    ToggleFullscreen,
    ToggleFullHeight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RendererModeRequest {
    Gpu,
    Soft,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchScope {
    CurrentFolder,
    EntireCurrentDrive,
    AllLocalDrives,
    Drive(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexBackend {
    Detecting,
    WalkDir,
    NtfsMft,
    NtfsUsnLive,
    Mixed,
}

impl IndexBackend {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Detecting => "detecting",
            Self::WalkDir => "dirwalk",
            Self::NtfsMft => "ntfs-mft",
            Self::NtfsUsnLive => "ntfs-usn-live",
            Self::Mixed => "mixed",
        }
    }

    pub(crate) fn live_updates(self) -> bool {
        matches!(self, Self::NtfsUsnLive)
    }
}

pub(crate) fn backend_status_color(backend: IndexBackend) -> Color {
    match backend {
        IndexBackend::NtfsUsnLive => Color::Rgb(117, 227, 140),
        IndexBackend::NtfsMft => Color::Rgb(130, 210, 255),
        IndexBackend::Mixed => Color::Rgb(255, 198, 92),
        IndexBackend::WalkDir => Color::Rgb(184, 184, 184),
        IndexBackend::Detecting => Color::Rgb(170, 170, 170),
    }
}

pub(crate) fn state_status_color(indexing_in_progress: bool) -> Color {
    if indexing_in_progress {
        Color::Rgb(255, 184, 76)
    } else {
        Color::Rgb(117, 227, 140)
    }
}

impl SearchScope {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::CurrentFolder => "current-folder".to_string(),
            Self::EntireCurrentDrive => "entire-current-drive".to_string(),
            Self::AllLocalDrives => "all-local-drives".to_string(),
            Self::Drive(letter) => format!("{}:", letter.to_ascii_uppercase()),
        }
    }
}

pub(crate) fn estimate_index_memory_bytes(items: &[SearchItem]) -> usize {
    let mut total = std::mem::size_of_val(items);
    for item in items {
        total += std::mem::size_of::<SearchItem>();
        total += item.path.len();
    }
    total
}

pub(crate) fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}
