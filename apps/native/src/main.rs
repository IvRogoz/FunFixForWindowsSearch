#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod commands;
mod indexing;
mod indexing_ntfs;
mod search;
mod search_worker;
mod storage;
mod ui;
mod update;
mod windowing;

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;
use std::{env, process::Command};
use std::{io::Write, sync::OnceLock};
use std::{sync::mpsc, thread};

#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::GlobalHotKeyManager;
use iced::keyboard::Event as KeyboardEvent;
use iced::widget;
use iced::widget::operation;
use iced::{Color, Font, Task};
use tray_icon::menu::{Menu, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use commands::{format_latest_window, parse_scope_directive, scope_arg_value};
use search::{contains_ascii_case_insensitive, file_name_from_path};
use search_worker::{SearchEvent, SearchWorkerMessage};
use storage::{load_persisted_scope, load_quick_help_dismissed, persist_scope};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT;

const PANEL_WIDTH: f32 = 980.0;
const PANEL_HEIGHT: f32 = 560.0;
const PANEL_WIDTH_RATIO: f32 = 0.5;
const VISIBLE_RESULTS_LIMIT: usize = 600;
const QUERY_DEBOUNCE_DELAY: Duration = Duration::from_millis(70);
const SEARCH_BATCH_SIZE: usize = 12_000;
const FILENAME_INDEX_BUILD_BATCH: usize = 1_000;
const DEFAULT_LATEST_WINDOW_SECS: i64 = 5 * 60;
const DELTA_REFRESH_COOLDOWN: Duration = Duration::from_millis(300);
const PANEL_ANIMATION_DURATION: Duration = Duration::from_millis(180);
const FILE_NAME_FONT_SIZE: u32 = 14;
const FILE_PATH_FONT_SIZE: u32 = 12;
const FILE_PATH_MAX_CHARS: usize = 86;
const MAX_INDEX_EVENTS_PER_TICK: usize = 2;
const MAX_SEARCH_EVENTS_PER_TICK: usize = 24;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const UNKNOWN_TS: i64 = i64::MIN;
const KEYBOARD_PAGE_JUMP: usize = 12;
const CONSOLAS_REGULAR: &[u8] = include_bytes!("../assets/fonts/consola.ttf");
const CONSOLAS_BOLD: &[u8] = include_bytes!("../assets/fonts/consolab.ttf");

static DEBUG_LOG_FILES: OnceLock<std::sync::Mutex<Vec<std::fs::File>>> = OnceLock::new();
static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();

fn main() -> iced::Result {
    let _ = DEBUG_ENABLED.set(env::var("WIZMINI_DEBUG").ok().as_deref() == Some("1"));
    let _ = init_debug_log_file();
    std::panic::set_hook(Box::new(|info| {
        debug_log(&format!("panic: {}", info));
    }));

    let start_visible = should_start_visible_from_args();

    iced::application(
        move || {
            let app = App::default();
            let should_start_visible = start_visible || app.show_quick_help_overlay;
            let initial_scope = app.scope.clone();

            let mut tasks = vec![Task::done(Message::StartIndex(initial_scope))];

            if should_start_visible {
                tasks.push(windowing::prepare_panel_for_show_mode());
                tasks.push(windowing::sync_window_to_progress(1.0));
                tasks.push(operation::focus(app.search_input_id.clone()));
                tasks.push(operation::move_cursor_to_end(app.search_input_id.clone()));
            } else {
                tasks.push(windowing::initialize_panel_hidden_mode());
            }

            let mut app = app;
            if should_start_visible {
                app.panel_visible = true;
                app.panel_progress = 1.0;
            }

            (app, Task::batch(tasks))
        },
        update::update,
        ui::view,
    )
    .title("WizMini")
    .font(CONSOLAS_REGULAR)
    .font(CONSOLAS_BOLD)
    .default_font(Font::with_name("Consolas"))
    .theme(ui::theme)
    .window(windowing::native_window_settings())
    .subscription(ui::subscription)
    .run()
}

fn should_start_visible_from_args() -> bool {
    env::args().any(|arg| arg == "--show")
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
        .join("wizmini-debug.log")
}

fn debug_log_path_exe_dir() -> std::path::PathBuf {
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|v| v.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    exe_dir.join("wizmini-debug.log")
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

#[derive(Debug, Clone)]
enum Message {
    QueryChanged(String),
    ActivateSelected,
    PollExternal,
    AnimateFrame,
    Keyboard(KeyboardEvent),
    StartIndex(SearchScope),
    CloseQuickHelp,
    CloseQuickHelpForever,
}

#[derive(Debug, Clone)]
struct SearchItem {
    path: Box<str>,
    modified_unix_secs: i64,
}

enum IndexEvent {
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchScope {
    CurrentFolder,
    EntireCurrentDrive,
    AllLocalDrives,
    Drive(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexBackend {
    Detecting,
    WalkDir,
    NtfsMft,
    NtfsUsnLive,
    Mixed,
}

impl IndexBackend {
    fn label(self) -> &'static str {
        match self {
            Self::Detecting => "detecting",
            Self::WalkDir => "dirwalk",
            Self::NtfsMft => "ntfs-mft",
            Self::NtfsUsnLive => "ntfs-usn-live",
            Self::Mixed => "mixed",
        }
    }

    fn live_updates(self) -> bool {
        matches!(self, Self::NtfsUsnLive)
    }
}

fn backend_status_color(backend: IndexBackend) -> Color {
    match backend {
        IndexBackend::NtfsUsnLive => Color::from_rgb8(117, 227, 140),
        IndexBackend::NtfsMft => Color::from_rgb8(130, 210, 255),
        IndexBackend::Mixed => Color::from_rgb8(255, 198, 92),
        IndexBackend::WalkDir => Color::from_rgb8(184, 184, 184),
        IndexBackend::Detecting => Color::from_rgb8(170, 170, 170),
    }
}

fn state_status_color(indexing_in_progress: bool) -> Color {
    if indexing_in_progress {
        Color::from_rgb8(255, 184, 76)
    } else {
        Color::from_rgb8(117, 227, 140)
    }
}

impl SearchScope {
    fn label(&self) -> String {
        match self {
            Self::CurrentFolder => "current-folder".to_string(),
            Self::EntireCurrentDrive => "entire-current-drive".to_string(),
            Self::AllLocalDrives => "all-local-drives".to_string(),
            Self::Drive(letter) => format!("{}:", letter.to_ascii_uppercase()),
        }
    }
}

struct App {
    raw_query: String,
    query: String,
    all_items: Vec<SearchItem>,
    items: Vec<SearchItem>,
    selected: usize,
    last_action: String,
    panel_visible: bool,
    panel_progress: f32,
    panel_anim_last_tick: Option<Instant>,
    _hotkey_manager: Option<GlobalHotKeyManager>,
    _hotkey: Option<HotKey>,
    _tray_icon: Option<TrayIcon>,
    menu_toggle_id: Option<MenuId>,
    menu_quit_id: Option<MenuId>,
    last_toggle_at: Option<Instant>,
    search_input_id: widget::Id,
    results_scroll_id: widget::Id,
    scope: SearchScope,
    command_selected: usize,
    index_rx: Option<mpsc::Receiver<IndexEvent>>,
    index_job_counter: u64,
    active_index_job: Option<u64>,
    indexing_in_progress: bool,
    indexing_progress: f32,
    indexing_phase: &'static str,
    index_backend: IndexBackend,
    index_memory_bytes: usize,
    visual_progress_test_active: bool,
    indexing_is_refresh: bool,
    is_elevated: bool,
    use_dirwalk_fallback: bool,
    show_privilege_overlay: bool,
    show_quick_help_overlay: bool,
    quick_help_selected_action: usize,
    pending_query: Option<(String, Instant, u64)>,
    query_edit_counter: u64,
    search_tx: mpsc::Sender<SearchWorkerMessage>,
    search_rx: mpsc::Receiver<SearchEvent>,
    search_generation: u64,
    active_search_job: Option<u64>,
    active_search_query: Option<String>,
    active_search_cursor: usize,
    filename_exact_index: HashMap<String, Vec<usize>>,
    filename_prefix_index: HashMap<String, Vec<usize>>,
    filename_index_dirty: bool,
    filename_index_building: bool,
    filename_index_build_cursor: usize,
    needs_search_refresh: bool,
    next_search_refresh_at: Instant,
    latest_only_mode: bool,
    latest_window_secs: i64,
    tracking_enabled: bool,
    recent_event_by_path: HashMap<Box<str>, i64>,
    changes_added_since_index: usize,
    changes_updated_since_index: usize,
    changes_deleted_since_index: usize,
    hotkey_retry_after: Option<Instant>,
    skip_scope_persist_once: bool,
}

impl Default for App {
    fn default() -> Self {
        let (tray_icon, menu_toggle_id, menu_quit_id) = init_tray().unwrap_or((None, None, None));
        let (hotkey_manager, hotkey, hotkey_retry_after) = match init_hotkey() {
            Ok((manager, hotkey)) => (manager, hotkey, None),
            Err(err) => {
                debug_log(&format!("init_hotkey failed: {}", err));
                (
                    None,
                    None,
                    Some(Instant::now() + Duration::from_millis(1200)),
                )
            }
        };
        let persisted_scope = load_persisted_scope();
        let is_elevated = is_process_elevated();
        let arg_scope_override = startup_scope_override_from_args();
        let (search_tx, search_rx) = search_worker::spawn_search_worker();
        let startup_scope = if let Some(scope) = arg_scope_override.clone() {
            scope
        } else if is_elevated {
            persisted_scope
        } else {
            SearchScope::CurrentFolder
        };

        let app = Self {
            raw_query: String::new(),
            query: String::new(),
            all_items: Vec::new(),
            items: Vec::new(),
            selected: 0,
            last_action: "Indexing files...".to_string(),
            panel_visible: false,
            panel_progress: 0.0,
            panel_anim_last_tick: None,
            _hotkey_manager: hotkey_manager,
            _hotkey: hotkey,
            _tray_icon: tray_icon,
            menu_toggle_id,
            menu_quit_id,
            last_toggle_at: None,
            search_input_id: widget::Id::new("search-input"),
            results_scroll_id: widget::Id::new("results-scroll"),
            scope: startup_scope,
            command_selected: 0,
            index_rx: None,
            index_job_counter: 0,
            active_index_job: None,
            indexing_in_progress: false,
            indexing_progress: 0.0,
            indexing_phase: "index",
            index_backend: IndexBackend::Detecting,
            index_memory_bytes: 0,
            visual_progress_test_active: false,
            indexing_is_refresh: false,
            is_elevated,
            use_dirwalk_fallback: !is_elevated,
            show_privilege_overlay: !is_elevated,
            show_quick_help_overlay: !load_quick_help_dismissed(),
            quick_help_selected_action: 0,
            pending_query: None,
            query_edit_counter: 0,
            search_tx,
            search_rx,
            search_generation: 0,
            active_search_job: None,
            active_search_query: None,
            active_search_cursor: 0,
            filename_exact_index: HashMap::new(),
            filename_prefix_index: HashMap::new(),
            filename_index_dirty: true,
            filename_index_building: false,
            filename_index_build_cursor: 0,
            needs_search_refresh: false,
            next_search_refresh_at: Instant::now(),
            latest_only_mode: false,
            latest_window_secs: DEFAULT_LATEST_WINDOW_SECS,
            tracking_enabled: true,
            recent_event_by_path: HashMap::new(),
            changes_added_since_index: 0,
            changes_updated_since_index: 0,
            changes_deleted_since_index: 0,
            hotkey_retry_after,
            skip_scope_persist_once: !is_elevated && arg_scope_override.is_none(),
        };

        app
    }
}

fn debug_log(message: &str) {
    if !*DEBUG_ENABLED.get_or_init(|| false) {
        return;
    }

    let line = format!("[wizmini-debug] {}\n", message);

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

#[cfg(target_os = "windows")]
fn is_process_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(target_os = "windows"))]
fn is_process_elevated() -> bool {
    true
}

#[cfg(target_os = "windows")]
fn request_self_elevation(scope: &SearchScope) -> Result<(), String> {
    let exe_path = env::current_exe().map_err(|e| e.to_string())?;
    let exe = to_wide(exe_path.to_string_lossy().as_ref());
    let verb = to_wide("runas");
    let params = to_wide(&format!("--show --scope={}", scope_arg_value(scope)));

    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWDEFAULT,
        )
    } as isize;

    if result <= 32 {
        Err(format!(
            "UAC elevation failed or cancelled (code {})",
            result
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn request_self_elevation(_scope: &SearchScope) -> Result<(), String> {
    Err("Elevation is only supported on Windows".to_string())
}

fn open_path(path: &str) -> Result<(), String> {
    Command::new("cmd")
        .args(["/C", "start", "", path])
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(())
}

fn reveal_path(path: &str) -> Result<(), String> {
    Command::new("explorer")
        .arg(format!("/select,{}", path))
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(target_os = "windows")]
fn to_wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

impl App {
    fn apply_raw_query(&mut self, raw_query: String, execute_directives: bool) -> Task<Message> {
        self.pending_query = None;
        self.needs_search_refresh = false;
        self.raw_query = raw_query;

        let parsed = parse_scope_directive(&self.raw_query);
        debug_log(&format!(
            "apply_raw_query execute={} raw='{}' clean='{}' scope_override={:?} latest_only={} latest_window_secs={:?} reindex_current_scope={} toggle_tracking={} test_progress={} exit_app={}",
            execute_directives,
            self.raw_query,
            parsed.clean_query,
            parsed.scope_override,
            parsed.latest_only,
            parsed.latest_window_secs,
            parsed.reindex_current_scope,
            parsed.toggle_tracking,
            parsed.test_progress,
            parsed.exit_app
        ));
        self.query = parsed.clean_query;

        if !execute_directives {
            let cmd = self.raw_query.trim_start();
            if !cmd.starts_with("/latest") && !cmd.starts_with("/last") {
                self.latest_only_mode = false;
            }
            self.schedule_search_from_current_query();
            return Task::none();
        }

        if parsed.test_progress {
            self.visual_progress_test_active = true;
            self.indexing_in_progress = true;
            self.indexing_progress = 0.0;
            self.last_action = "Running visual progress test".to_string();
            return Task::none();
        }

        if parsed.exit_app {
            return iced::exit();
        }

        if parsed.elevate_app {
            if self.is_elevated {
                self.last_action = "Already elevated".to_string();
                return Task::none();
            }

            match request_self_elevation(&SearchScope::EntireCurrentDrive) {
                Ok(()) => return iced::exit(),
                Err(err) => {
                    self.last_action = err;
                    return Task::none();
                }
            }
        }

        if parsed.latest_only {
            if !self.tracking_enabled {
                self.last_action = "Tracking is off (use /track to enable)".to_string();
                return Task::none();
            }

            self.latest_only_mode = true;
            if let Some(window_secs) = parsed.latest_window_secs {
                self.latest_window_secs = window_secs;
            }
            self.query.clear();
            self.last_action = format!(
                "Showing files changed in last {}",
                format_latest_window(self.latest_window_secs)
            );
            self.schedule_search_from_current_query();
            return Task::none();
        }

        if parsed.toggle_tracking {
            self.tracking_enabled = !self.tracking_enabled;
            self.latest_only_mode = false;
            self.recent_event_by_path.clear();
            if self.tracking_enabled {
                self.last_action = "Tracking enabled".to_string();
            } else {
                self.last_action = "Tracking disabled".to_string();
                self.changes_added_since_index = 0;
                self.changes_updated_since_index = 0;
                self.changes_deleted_since_index = 0;
            }
            return Task::none();
        }

        if parsed.reindex_current_scope {
            self.latest_only_mode = false;
            self.query.clear();
            self.last_action = format!("Reindexing scope: {}", self.scope.label());
            return Task::done(Message::StartIndex(self.scope.clone()));
        }

        let cmd = self.raw_query.trim_start();
        if !cmd.starts_with("/latest") && !cmd.starts_with("/last") {
            self.latest_only_mode = false;
        }

        if let Some(new_scope) = parsed.scope_override {
            if self.indexing_in_progress && self.scope == new_scope {
                self.last_action = format!("Already indexing scope: {}", self.scope.label());
                return Task::none();
            }

            self.scope = new_scope;
            self.all_items.clear();
            self.items.clear();
            self.selected = 0;
            self.last_action = format!("Indexing scope: {}", self.scope.label());
            debug_log(&format!(
                "apply_raw_query starting index for scope={} ",
                self.scope.label()
            ));
            return Task::done(Message::StartIndex(self.scope.clone()));
        }

        self.schedule_search_from_current_query();
        Task::none()
    }

    fn begin_index(&mut self, scope: SearchScope) {
        self.index_job_counter += 1;
        let job_id = self.index_job_counter;
        debug_log(&format!(
            "begin_index job_id={} scope={}",
            job_id,
            scope.label()
        ));
        self.active_index_job = Some(job_id);
        self.scope = scope.clone();
        if self.skip_scope_persist_once {
            self.skip_scope_persist_once = false;
        } else {
            persist_scope(&self.scope);
        }
        self.visual_progress_test_active = false;
        self.indexing_in_progress = true;
        self.indexing_progress = 0.0;
        self.indexing_phase = "index";
        self.indexing_is_refresh = false;
        self.index_backend = IndexBackend::Detecting;
        self.index_memory_bytes = 0;
        self.filename_index_dirty = true;
        self.filename_index_building = false;
        self.filename_index_build_cursor = 0;
        self.cancel_active_search();
        let _ = self.search_tx.send(SearchWorkerMessage::Clear);
        self.needs_search_refresh = false;
        self.recent_event_by_path.clear();
        self.changes_added_since_index = 0;
        self.changes_updated_since_index = 0;
        self.changes_deleted_since_index = 0;

        let (tx, rx) = mpsc::channel::<IndexEvent>();
        self.index_rx = Some(rx);

        let allow_dirwalk_fallback = self.use_dirwalk_fallback;
        thread::spawn(move || {
            indexing::run_index_job(scope, job_id, tx, allow_dirwalk_fallback);
        });
    }

    fn recompute_index_memory_bytes(&mut self) {
        self.index_memory_bytes = estimate_index_memory_bytes(&self.all_items);
    }

    fn push_corpus_to_search_worker(&self) {
        let _ = self.search_tx.send(SearchWorkerMessage::SetCorpus {
            items: self.all_items.clone(),
            recent_event_by_path: self.recent_event_by_path.clone(),
        });
    }

    fn cancel_active_search(&mut self) {
        self.active_search_job = None;
        self.active_search_query = None;
        self.active_search_cursor = 0;
        let _ = self.search_tx.send(SearchWorkerMessage::Cancel);
    }

    fn schedule_search_from_current_query(&mut self) {
        if !self.panel_visible {
            self.cancel_active_search();
            return;
        }

        let q = self.query.trim().to_ascii_lowercase();

        if q.is_empty() && !self.latest_only_mode {
            self.items = self
                .all_items
                .iter()
                .take(VISIBLE_RESULTS_LIMIT)
                .cloned()
                .collect();
            self.cancel_active_search();
            self.clamp_selected();
        } else {
            if !self.latest_only_mode {
                if let Some(results) = self.try_fast_filename_search(&q) {
                    self.items = results;
                    self.cancel_active_search();
                    self.clamp_selected();
                    return;
                }
            }

            self.search_generation = self.search_generation.wrapping_add(1);
            let generation = self.search_generation;
            self.active_search_job = Some(generation);
            self.active_search_query = Some(q);
            self.active_search_cursor = 0;
            let _ = self.search_tx.send(SearchWorkerMessage::Run {
                generation,
                query: self.query.trim().to_ascii_lowercase(),
                latest_only_mode: self.latest_only_mode,
                latest_window_secs: self.latest_window_secs,
            });
        }
    }

    fn process_filename_index_build_step(&mut self) {
        if !self.filename_index_dirty {
            return;
        }

        if !self.filename_index_building {
            self.filename_exact_index.clear();
            self.filename_prefix_index.clear();
            self.filename_index_build_cursor = 0;
            self.filename_index_building = true;
        }

        let end = (self.filename_index_build_cursor + FILENAME_INDEX_BUILD_BATCH)
            .min(self.all_items.len());
        for index in self.filename_index_build_cursor..end {
            let item = &self.all_items[index];
            let name_lower = file_name_from_path(item.path.as_ref()).to_ascii_lowercase();
            self.filename_exact_index
                .entry(name_lower.clone())
                .or_default()
                .push(index);

            let mut prefix = String::new();
            for ch in name_lower.chars().take(3) {
                prefix.push(ch);
                self.filename_prefix_index
                    .entry(prefix.clone())
                    .or_default()
                    .push(index);
            }
        }

        self.filename_index_build_cursor = end;
        if self.filename_index_build_cursor >= self.all_items.len() {
            self.filename_index_dirty = false;
            self.filename_index_building = false;
            self.filename_index_build_cursor = 0;
        }
    }

    fn try_fast_filename_search(&mut self, query_lower: &str) -> Option<Vec<SearchItem>> {
        if query_lower.is_empty()
            || query_lower.contains('*')
            || query_lower.contains('?')
            || query_lower.contains('\\')
            || query_lower.contains('/')
            || query_lower.contains(':')
        {
            return None;
        }

        if self.filename_index_dirty || self.filename_index_building {
            return None;
        }

        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();

        if let Some(exact) = self.filename_exact_index.get(query_lower) {
            for &idx in exact {
                if seen.insert(idx) {
                    out.push(self.all_items[idx].clone());
                    if out.len() >= VISIBLE_RESULTS_LIMIT {
                        return Some(out);
                    }
                }
            }
        }

        let mut prefix_key = String::new();
        for ch in query_lower.chars().take(3) {
            prefix_key.push(ch);
        }

        if let Some(candidates) = self.filename_prefix_index.get(&prefix_key) {
            for &idx in candidates {
                if seen.contains(&idx) {
                    continue;
                }

                let name = file_name_from_path(self.all_items[idx].path.as_ref());
                if contains_ascii_case_insensitive(name, query_lower) {
                    seen.insert(idx);
                    out.push(self.all_items[idx].clone());
                    if out.len() >= VISIBLE_RESULTS_LIMIT {
                        break;
                    }
                }
            }
        }

        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn clamp_selected(&mut self) {
        if self.items.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.items.len() - 1);
        }
    }

    fn apply_index_delta(
        &mut self,
        upserts: Vec<SearchItem>,
        deleted_paths: Vec<String>,
    ) -> (usize, usize, usize) {
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut deleted_count = 0usize;
        if !deleted_paths.is_empty() {
            let delete_set: std::collections::HashSet<String> = deleted_paths.into_iter().collect();
            if self.tracking_enabled {
                deleted_count = delete_set.len();
                for path in &delete_set {
                    self.recent_event_by_path.remove(path.as_str());
                }
            }
            self.all_items
                .retain(|item| !delete_set.contains(item.path.as_ref()));
        }

        let mut added_count = 0usize;
        let mut updated_count = 0usize;
        for upsert in upserts {
            if self.tracking_enabled {
                let event_ts = if upsert.modified_unix_secs == UNKNOWN_TS {
                    now_unix
                } else {
                    upsert.modified_unix_secs
                };
                self.recent_event_by_path
                    .insert(upsert.path.clone(), event_ts);
            }
            if let Some(existing) = self
                .all_items
                .iter_mut()
                .find(|item| item.path == upsert.path)
            {
                *existing = upsert;
                if self.tracking_enabled {
                    updated_count += 1;
                }
            } else {
                self.all_items.push(upsert);
                if self.tracking_enabled {
                    added_count += 1;
                }
            }
        }

        self.needs_search_refresh = self.latest_only_mode || self.query.trim().is_empty();
        self.filename_index_dirty = true;
        self.filename_index_building = false;
        self.filename_index_build_cursor = 0;
        (added_count, updated_count, deleted_count)
    }
}

fn estimate_index_memory_bytes(items: &[SearchItem]) -> usize {
    let mut total = std::mem::size_of_val(items);
    for item in items {
        total += std::mem::size_of::<SearchItem>();
        total += item.path.len();
    }
    total
}

fn format_bytes(bytes: usize) -> String {
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

fn init_hotkey() -> Result<(Option<GlobalHotKeyManager>, Option<HotKey>), String> {
    let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
    let hotkey = HotKey::new(Some(Modifiers::empty()), Code::Backquote);

    manager.register(hotkey).map_err(|e| e.to_string())?;

    Ok((Some(manager), Some(hotkey)))
}

fn init_tray() -> Result<(Option<TrayIcon>, Option<MenuId>, Option<MenuId>), String> {
    let icon = build_tray_icon()?;
    let menu = Menu::new();
    let toggle = MenuItem::new("Show/Hide", true, None);
    let quit = MenuItem::new("Quit", true, None);

    menu.append(&toggle).map_err(|e| e.to_string())?;
    menu.append(&quit).map_err(|e| e.to_string())?;

    let tray = TrayIconBuilder::new()
        .with_tooltip("WizMini")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| e.to_string())?;

    Ok((
        Some(tray),
        Some(toggle.id().clone()),
        Some(quit.id().clone()),
    ))
}

fn build_tray_icon() -> Result<Icon, String> {
    let width = 16;
    let height = 16;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);

    for y in 0..height {
        for x in 0..width {
            let edge = x == 0 || x == width - 1 || y == 0 || y == height - 1;
            let body = x == y || (x + y) == (width - 1);

            let (r, g, b, a) = if edge {
                (26, 35, 46, 255)
            } else if body {
                (125, 207, 255, 255)
            } else {
                (15, 19, 24, 255)
            };

            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    Icon::from_rgba(rgba, width, height).map_err(|e| e.to_string())
}
